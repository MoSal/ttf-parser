use std::ops::Range;

use crate::parser::{Stream, FromData, LazyArray, Offset32};
use crate::{Font, GlyphId, TableName, Result, Error};


impl<'a> Font<'a> {
    /// Resolves Glyph ID for code point.
    ///
    /// Returns `Error::NoGlyph` instead of `0` when glyph is not found.
    pub fn glyph_index(&self, c: char) -> Result<GlyphId> {
        let cmap_data = self.table_data(TableName::CharacterToGlyphIndexMapping)?;
        let mut s = Stream::new(cmap_data);
        s.skip_u16(); // version
        let num_tables = s.read_u16();

        for _ in 0..num_tables {
            s.skip_u16(); // platform_id
            s.skip_u16(); // encoding_id
            let offset = s.read_u32() as usize;

            let subtable_data = &cmap_data[offset..];
            let mut s = Stream::new(subtable_data);
            let format = match parse_format(s.read_u16()) {
                Some(format) => format,
                None => continue,
            };

            let c = c as u32;
            let glyph = match format {
                Format::ByteEncodingTable => {
                    parse_byte_encoding_table(&mut s, c)
                }
                Format::HighByteMappingThroughTable => {
                    parse_high_byte_mapping_through_table(subtable_data, c)
                }
                Format::SegmentMappingToDeltaValues => {
                    parse_segment_mapping_to_delta_values(subtable_data, c)
                }
                Format::SegmentedCoverage | Format::ManyToOneRangeMappings => {
                    parse_segmented_coverage(&mut s, c, format)
                }
                Format::UnicodeVariationSequences => {
                    // This subtable is used only by glyph_variation_index().
                    continue;
                }
                _ => continue,
            };

            if let Some(id) = glyph {
                return Ok(GlyphId(id));
            }
        }

        Err(Error::NoGlyph)
    }

    /// Resolves a variation of a Glyph ID from two code points.
    ///
    /// Implemented according to
    /// [Unicode Variation Sequences](
    /// https://docs.microsoft.com/en-us/typography/opentype/spec/cmap#format-14-unicode-variation-sequences).
    ///
    /// Returns `Error::NoGlyph` instead of `0` when glyph is not found.
    pub fn glyph_variation_index(&self, c: char, variation: char) -> Result<GlyphId> {
        let cmap_data = self.table_data(TableName::CharacterToGlyphIndexMapping)?;
        let mut s = Stream::new(cmap_data);
        s.skip_u16(); // version
        let num_tables = s.read_u16();

        for _ in 0..num_tables {
            s.skip_u16(); // platform_id
            s.skip_u16(); // encoding_id
            let offset = s.read_u32() as usize;

            let subtable_data = &cmap_data[offset..];
            let mut s = Stream::new(subtable_data);
            let format = match parse_format(s.read_u16()) {
                Some(format) => format,
                None => continue,
            };

            if format != Format::UnicodeVariationSequences {
                continue;
            }

            return self.parse_unicode_variation_sequences(subtable_data, c, variation as u32);
        }

        Err(Error::NoGlyph)
    }

    fn parse_unicode_variation_sequences(
        &self,
        data: &[u8],
        c: char,
        variation: u32,
    ) -> Result<GlyphId> {
        let cp = c as u32;

        let mut s = Stream::new(data);
        s.skip_u16(); // format
        s.skip_u32(); // length
        let num_var_selector_records = s.read_u32() as usize;
        let records = s.read_array::<VariationSelectorRecord>(num_var_selector_records);

        let record = records.binary_search_by(|v| v.variation.cmp(&variation)).ok_or(Error::NoGlyph)?;

        if let Some(offset) = record.default_uvs_offset {
            let mut s = Stream::new(&data[offset.0 as usize..]);
            let count: u32 = s.read(); // numUnicodeValueRanges
            let ranges: LazyArray<UnicodeRangeRecord> = s.read_array(count as usize);
            for range in ranges {
                if range.contains(c) {
                    // This is a default glyph.
                    return self.glyph_index(c);
                }
            }
        }

        if let Some(offset) = record.non_default_uvs_offset {
            let mut s = Stream::new(&data[offset.0 as usize..]);
            let count: u32 = s.read(); // numUVSMappings
            let uvs_mappings: LazyArray<UVSMappingRecord> = s.read_array(count as usize);
            if let Some(mapping) = uvs_mappings.binary_search_by(|v| v.unicode_value.cmp(&cp)) {
                return Ok(mapping.glyph);
            }
        }

        Err(Error::NoGlyph)
    }
}

// https://docs.microsoft.com/en-us/typography/opentype/spec/cmap#format-0-byte-encoding-table
fn parse_byte_encoding_table(s: &mut Stream, code_point: u32) -> Option<u16> {
    let length = s.read_u16();
    s.skip_u16(); // language

    if code_point < (length as u32) {
        s.skip(code_point as usize);
        Some(s.read_u8() as u16)
    } else {
        None
    }
}

// This table has a pretty complex parsing algorithm.
// A detailed explanation can be found here:
// https://docs.microsoft.com/en-us/typography/opentype/spec/cmap#format-2-high-byte-mapping-through-table
// https://developer.apple.com/fonts/TrueType-Reference-Manual/RM06/Chap6cmap.html
// https://github.com/fonttools/fonttools/blob/a360252709a3d65f899915db0a5bd753007fdbb7/Lib/fontTools/ttLib/tables/_c_m_a_p.py#L360
fn parse_high_byte_mapping_through_table(data: &[u8], code_point: u32) -> Option<u16> {
    // This subtable supports code points only in a u16 range.
    if code_point > 0xffff {
        return None;
    }

    let code_point = code_point as u16;
    let high_byte = (code_point >> 8) as u16;
    let low_byte = (code_point & 0x00FF) as u16;

    let mut s = Stream::new(data);
    s.skip_u16(); // format
    s.skip_u16(); // length
    s.skip_u16(); // language
    let sub_header_keys: LazyArray<u16> = s.read_array(256);
    // The maximum index in a sub_header_keys is a sub_headers count.
    let sub_headers_count = sub_header_keys.into_iter().map(|n| n / 8).max()? + 1;
    // Remember sub_headers offset before reading. Will be used later.
    let sub_headers_offset = s.offset();
    let sub_headers: LazyArray<SubHeaderRecord> = s.read_array(sub_headers_count as usize);

    let i = if code_point < 0xff {
        // 'SubHeader 0 is special: it is used for single-byte character codes.'
        0
    } else {
        // 'Array that maps high bytes to subHeaders: value is subHeader index × 8.'
        (sub_header_keys.at(high_byte as usize) / 8) as usize
    };

    let sub_header = sub_headers.at(i);

    let range_end = sub_header.first_code + sub_header.entry_count;
    if low_byte < sub_header.first_code || low_byte > range_end {
        return None;
    }

    // SubHeaderRecord::id_range_offset points to SubHeaderRecord::first_code
    // in the glyphIndexArray. So we have to advance to our code point.
    let index_offset = (low_byte - sub_header.first_code) as usize * u16::size_of();

    // 'The value of the idRangeOffset is the number of bytes
    // past the actual location of the idRangeOffset'.
    let offset =
          sub_headers_offset
        // Advance to required subheader.
        + SubHeaderRecord::size_of() * (i + 1)
        // Move back to idRangeOffset start.
        - u16::size_of()
        // Use defined offset.
        + sub_header.id_range_offset as usize
        // Advance to required index in the glyphIndexArray.
        + index_offset;

    let glyph: u16 = Stream::read_at(data, offset);
    if glyph == 0 {
        return None;
    }

    let glyph = ((glyph as i32 + sub_header.id_delta as i32) % 65536) as u16;
    Some(glyph)
}

// https://docs.microsoft.com/en-us/typography/opentype/spec/cmap#format-4-segment-mapping-to-delta-values
fn parse_segment_mapping_to_delta_values(data: &[u8], code_point: u32) -> Option<u16> {
    // This subtable supports code points only in a u16 range.
    if code_point > 0xffff {
        return None;
    }

    let code_point = code_point as u16;

    let mut s = Stream::new(data);
    s.skip_u16(); // format
    s.skip_u16(); // length
    s.skip_u16(); // language
    let seg_count_x2 = s.read_u16() as usize;
    let seg_count = seg_count_x2 / 2;
    s.skip_u16(); // searchRange
    s.skip_u16(); // entrySelector
    s.skip_u16(); // rangeShift
    let end_codes = s.read_array::<u16>(seg_count);
    s.skip_u16(); // reservedPad
    let start_codes = s.read_array::<u16>(seg_count);
    let id_deltas = s.read_array::<i16>(seg_count);
    let id_range_offset_pos = s.offset();
    let id_range_offsets = s.read_array::<u16>(seg_count);

    // A custom binary search.
    let mut start = 0;
    let mut end = seg_count;
    while end > start {
        let index = (start + end) / 2;
        let end_value = end_codes.at(index);
        if end_value >= code_point {
            let start_value = start_codes.at(index);
            if start_value > code_point {
                end = index;
            } else {
                let id_range_offset = id_range_offsets.at(index);
                let id_delta = id_deltas.at(index);
                if id_range_offset == 0 {
                    return Some(code_point.wrapping_add(id_delta as u16));
                }

                let delta = (code_point - start_value) * 2;
                let id_range_offset_pos = (id_range_offset_pos + index * 2) as u16;
                let pos = id_range_offset_pos.wrapping_add(delta) + id_range_offset;
                let glyph_array_value: u16 = Stream::read_at(data, pos as usize);
                if glyph_array_value == 0 {
                    return None;
                }

                let glyph_id = (glyph_array_value as i16).wrapping_add(id_delta);
                return Some(glyph_id as u16);
            }
        } else {
            start = index + 1;
        }
    }

    None
}

// + ManyToOneRangeMappings
// https://docs.microsoft.com/en-us/typography/opentype/spec/cmap#format-12-segmented-coverage
// https://docs.microsoft.com/en-us/typography/opentype/spec/cmap#format-13-many-to-one-range-mappings
fn parse_segmented_coverage(s: &mut Stream, code_point: u32, format: Format) -> Option<u16> {
    s.skip_u16(); // reserved
    s.skip_u32(); // length
    s.skip_u32(); // language
    let num_groups = s.read_u32() as usize;
    let groups = s.read_array::<SequentialMapGroup>(num_groups);
    for group in groups {
        if group.char_code_range.contains(&code_point) {
            if format == Format::SegmentedCoverage {
                let id = group.start_glyph_id + code_point - group.char_code_range.start;
                return Some(id as u16);
            } else {
                return Some(group.start_glyph_id as u16);
            }
        }
    }

    None
}


#[derive(Clone, Copy, PartialEq, Debug)]
enum Format {
    ByteEncodingTable = 0,
    HighByteMappingThroughTable = 2,
    SegmentMappingToDeltaValues = 4,
    TrimmedTableMapping = 6,
    MixedCoverage = 8,
    TrimmedArray = 10,
    SegmentedCoverage = 12,
    ManyToOneRangeMappings = 13,
    UnicodeVariationSequences = 14,
}

fn parse_format(v: u16) -> Option<Format> {
    match v {
         0 => Some(Format::ByteEncodingTable),
         2 => Some(Format::HighByteMappingThroughTable),
         4 => Some(Format::SegmentMappingToDeltaValues),
         6 => Some(Format::TrimmedTableMapping),
         8 => Some(Format::MixedCoverage),
        10 => Some(Format::TrimmedArray),
        12 => Some(Format::SegmentedCoverage),
        13 => Some(Format::ManyToOneRangeMappings),
        14 => Some(Format::UnicodeVariationSequences),
        _ => None,
    }
}


#[derive(Clone, Copy, Debug)]
struct SubHeaderRecord {
    first_code: u16,
    entry_count: u16,
    id_delta: i16,
    id_range_offset: u16,
}

impl FromData for SubHeaderRecord {
    fn parse(data: &[u8]) -> Self {
        let mut s = Stream::new(data);
        SubHeaderRecord {
            first_code: s.read(),
            entry_count: s.read(),
            id_delta: s.read(),
            id_range_offset: s.read(),
        }
    }
}


// Also, the same as ConstantMapGroup.
#[derive(Debug)]
struct SequentialMapGroup {
    char_code_range: Range<u32>,
    start_glyph_id: u32,
}

impl FromData for SequentialMapGroup {
    fn parse(data: &[u8]) -> Self {
        let mut s = Stream::new(data);
        SequentialMapGroup {
            // Make the upper bound inclusive.
            char_code_range: s.read_u32()..(s.read_u32() + 1),
            start_glyph_id: s.read_u32(),
        }
    }
}


struct VariationSelectorRecord {
    variation: u32,
    default_uvs_offset: Option<Offset32>,
    non_default_uvs_offset: Option<Offset32>,
}

impl FromData for VariationSelectorRecord {
    fn parse(data: &[u8]) -> Self {
        let mut s = Stream::new(data);
        VariationSelectorRecord {
            variation: s.read_u24(),
            default_uvs_offset: s.read(),
            non_default_uvs_offset: s.read(),
        }
    }

    fn size_of() -> usize {
        // variation_selector is u24.
        3 + Offset32::size_of() + Offset32::size_of()
    }
}


struct UnicodeRangeRecord {
    start_unicode_value: u32,
    additional_count: u8,
}

impl UnicodeRangeRecord {
    fn contains(&self, c: char) -> bool {
        let end = self.start_unicode_value + self.additional_count as u32;
        self.start_unicode_value >= (c as u32) && (c as u32) < end
    }
}

impl FromData for UnicodeRangeRecord {
    fn parse(data: &[u8]) -> Self {
        let mut s = Stream::new(data);
        UnicodeRangeRecord {
            start_unicode_value: s.read_u24(),
            additional_count: s.read(),
        }
    }

    fn size_of() -> usize {
        // start_unicode_value is u24.
        3 + 1
    }
}


struct UVSMappingRecord {
    unicode_value: u32,
    glyph: GlyphId,
}

impl FromData for UVSMappingRecord {
    fn parse(data: &[u8]) -> Self {
        let mut s = Stream::new(data);
        UVSMappingRecord {
            unicode_value: s.read_u24(),
            glyph: s.read(),
        }
    }

    fn size_of() -> usize {
        // unicode_value is u24.
        3 + GlyphId::size_of()
    }
}
