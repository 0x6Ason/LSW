// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;

const PE_OFFSET: usize = 0x80;
const OPTIONAL_OFFSET: usize = PE_OFFSET + 4 + COFF_HEADER_BYTES;
const SECTION_OFFSET: usize = OPTIONAL_OFFSET + 0xf0;
const RAW_OFFSET: usize = 0x200;
const SECTION_RVA: u32 = 0x1000;

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn rva_offset(rva: u32) -> usize {
    RAW_OFFSET + usize::try_from(rva - SECTION_RVA).unwrap()
}

fn sample_pe64() -> Vec<u8> {
    let mut bytes = vec![0u8; 0x800];
    bytes[0..2].copy_from_slice(b"MZ");
    put_u32(&mut bytes, 0x3c, PE_OFFSET as u32);
    bytes[PE_OFFSET..PE_OFFSET + 4].copy_from_slice(b"PE\0\0");
    let coff = PE_OFFSET + 4;
    put_u16(&mut bytes, coff, 0x8664);
    put_u16(&mut bytes, coff + 2, 1);
    put_u32(&mut bytes, coff + 4, 0x1234_5678);
    put_u16(&mut bytes, coff + 16, 0xf0);
    put_u16(&mut bytes, coff + 18, 0x0022);

    put_u16(&mut bytes, OPTIONAL_OFFSET, 0x020b);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 16, 0x1100);
    put_u64(&mut bytes, OPTIONAL_OFFSET + 24, 0x0000_0001_4000_0000);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 56, 0x2000);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 60, 0x200);
    put_u16(&mut bytes, OPTIONAL_OFFSET + 68, 3);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 108, 16);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 112 + 8, 0x1100);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 112 + 12, 40);

    bytes[SECTION_OFFSET..SECTION_OFFSET + 6].copy_from_slice(b".rdata");
    put_u32(&mut bytes, SECTION_OFFSET + 8, 0x600);
    put_u32(&mut bytes, SECTION_OFFSET + 12, SECTION_RVA);
    put_u32(&mut bytes, SECTION_OFFSET + 16, 0x600);
    put_u32(&mut bytes, SECTION_OFFSET + 20, RAW_OFFSET as u32);
    put_u32(&mut bytes, SECTION_OFFSET + 36, 0x4000_0040);

    let descriptor = rva_offset(0x1100);
    put_u32(&mut bytes, descriptor, 0x1200);
    put_u32(&mut bytes, descriptor + 12, 0x1180);
    put_u32(&mut bytes, descriptor + 16, 0x1200);
    bytes[rva_offset(0x1180)..rva_offset(0x1180) + 13].copy_from_slice(b"KERNEL32.dll\0");
    let thunk = rva_offset(0x1200);
    put_u64(&mut bytes, thunk, 0x1300);
    put_u64(&mut bytes, thunk + 8, 0x8000_0000_0000_002a);
    put_u64(&mut bytes, thunk + 16, 0);
    let import_name = rva_offset(0x1300);
    put_u16(&mut bytes, import_name, 7);
    bytes[import_name + 2..import_name + 14].copy_from_slice(b"CreateFileW\0");
    bytes
}

fn sample_pe32() -> Vec<u8> {
    const PE32_OPTIONAL_SIZE: usize = 0xe0;
    const PE32_SECTION_OFFSET: usize = OPTIONAL_OFFSET + PE32_OPTIONAL_SIZE;

    let mut bytes = vec![0u8; 0x800];
    bytes[0..2].copy_from_slice(b"MZ");
    put_u32(&mut bytes, 0x3c, PE_OFFSET as u32);
    bytes[PE_OFFSET..PE_OFFSET + 4].copy_from_slice(b"PE\0\0");
    let coff = PE_OFFSET + 4;
    put_u16(&mut bytes, coff, 0x014c);
    put_u16(&mut bytes, coff + 2, 1);
    put_u16(&mut bytes, coff + 16, PE32_OPTIONAL_SIZE as u16);
    put_u16(&mut bytes, coff + 18, 0x0102);

    put_u16(&mut bytes, OPTIONAL_OFFSET, 0x010b);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 16, 0x1100);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 28, 0x0040_0000);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 56, 0x2000);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 60, 0x200);
    put_u16(&mut bytes, OPTIONAL_OFFSET + 68, 3);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 92, 16);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 96 + 8, 0x1100);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 96 + 12, 40);

    bytes[PE32_SECTION_OFFSET..PE32_SECTION_OFFSET + 6].copy_from_slice(b".rdata");
    put_u32(&mut bytes, PE32_SECTION_OFFSET + 8, 0x600);
    put_u32(&mut bytes, PE32_SECTION_OFFSET + 12, SECTION_RVA);
    put_u32(&mut bytes, PE32_SECTION_OFFSET + 16, 0x600);
    put_u32(&mut bytes, PE32_SECTION_OFFSET + 20, RAW_OFFSET as u32);
    put_u32(&mut bytes, PE32_SECTION_OFFSET + 36, 0x4000_0040);

    let descriptor = rva_offset(0x1100);
    put_u32(&mut bytes, descriptor, 0x1200);
    put_u32(&mut bytes, descriptor + 12, 0x1180);
    put_u32(&mut bytes, descriptor + 16, 0x1200);
    bytes[rva_offset(0x1180)..rva_offset(0x1180) + 13].copy_from_slice(b"KERNEL32.dll\0");
    let thunk = rva_offset(0x1200);
    put_u32(&mut bytes, thunk, 0x1300);
    put_u32(&mut bytes, thunk + 4, 0x8000_002a);
    let import_name = rva_offset(0x1300);
    put_u16(&mut bytes, import_name, 3);
    bytes[import_name + 2..import_name + 14].copy_from_slice(b"CreateFileA\0");
    bytes
}

#[test]
fn parses_pe64_imports() {
    let image = PeImage::parse(&sample_pe64()).unwrap();
    assert_eq!(image.kind, PeKind::Pe32Plus);
    assert_eq!(image.machine, PeMachine::X86_64);
    assert_eq!(image.subsystem, PeSubsystem::WindowsConsole);
    assert_eq!(image.entry_point_rva, 0x1100);
    assert_eq!(image.image_base, 0x0000_0001_4000_0000);
    assert_eq!(image.imported_symbol_count(), 2);
    assert_eq!(image.imports[0].dll, "KERNEL32.dll");
    assert_eq!(
        image.imports[0].symbols,
        vec![
            PeImportSymbol::Name {
                hint: 7,
                name: "CreateFileW".to_owned(),
            },
            PeImportSymbol::Ordinal(42),
        ]
    );
    assert_eq!(image.assess_for_beta().level, PeSupportLevel::Supported);
}

#[test]
fn parses_pe32_imports_and_marks_wow64_as_conditional() {
    let image = PeImage::parse(&sample_pe32()).expect("PE32 image should parse");
    assert_eq!(image.kind, PeKind::Pe32);
    assert_eq!(image.machine, PeMachine::X86);
    assert_eq!(image.image_base, 0x0040_0000);
    assert_eq!(image.imported_symbol_count(), 2);
    assert_eq!(
        image.imports[0].symbols,
        vec![
            PeImportSymbol::Name {
                hint: 3,
                name: "CreateFileA".to_owned(),
            },
            PeImportSymbol::Ordinal(42),
        ]
    );
    assert_eq!(image.assess_for_beta().level, PeSupportLevel::Conditional);
}

#[test]
fn arm_machine_codes_remain_distinct_in_reports() {
    for (raw, expected) in [
        (0x01c0, PeMachine::Arm),
        (0x01c2, PeMachine::Thumb),
        (0x01c4, PeMachine::ArmNt),
    ] {
        assert_eq!(PeMachine::from_raw(raw), expected);
        assert_eq!(expected.raw(), raw);
    }
}

#[test]
fn detects_managed_images_and_certificate_tables() {
    let mut bytes = sample_pe64();
    put_u32(&mut bytes, OPTIONAL_OFFSET + 112 + 14 * 8, 0x1400);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 112 + 14 * 8 + 4, 0x48);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 112 + 4 * 8, 0x700);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 112 + 4 * 8 + 4, 0x20);
    let image = PeImage::parse(&bytes).unwrap();
    assert!(image.is_managed);
    assert!(image.has_certificate_table);
    assert_eq!(image.assess_for_beta().level, PeSupportLevel::Conditional);
}

#[test]
fn rejects_truncated_clr_headers() {
    let mut bytes = sample_pe64();
    put_u32(&mut bytes, OPTIONAL_OFFSET + 112 + 14 * 8, 0x1400);
    put_u32(&mut bytes, OPTIONAL_OFFSET + 112 + 14 * 8 + 4, 1);
    assert!(PeImage::parse(&bytes).is_err());
}

#[test]
fn reports_gui_as_conditional_and_arm_as_unsupported() {
    let mut bytes = sample_pe64();
    put_u16(&mut bytes, OPTIONAL_OFFSET + 68, 2);
    let gui = PeImage::parse(&bytes).unwrap();
    assert_eq!(gui.assess_for_beta().level, PeSupportLevel::Conditional);

    put_u16(&mut bytes, OPTIONAL_OFFSET + 68, 7);
    let posix = PeImage::parse(&bytes).unwrap();
    assert_eq!(posix.assess_for_beta().level, PeSupportLevel::Unsupported);

    put_u16(&mut bytes, PE_OFFSET + 4, 0xaa64);
    let arm = PeImage::parse(&bytes).unwrap();
    assert_eq!(arm.assess_for_beta().level, PeSupportLevel::Unsupported);
}

#[test]
fn rejects_truncated_and_unmapped_inputs() {
    assert!(PeImage::parse(b"MZ").is_err());
    let mut bytes = sample_pe64();
    put_u32(&mut bytes, OPTIONAL_OFFSET + 112 + 8, 0x9000);
    assert!(PeImage::parse(&bytes).is_err());
}

#[test]
fn rejects_import_tables_without_a_terminator() {
    let mut bytes = sample_pe64();
    put_u32(&mut bytes, OPTIONAL_OFFSET + 112 + 12, 20);
    assert!(PeImage::parse(&bytes).is_err());
}

#[test]
fn aggregate_import_budget_rejects_symbol_and_string_exhaustion() {
    let mut budget = ImportBudget {
        symbols_remaining: 1,
        string_bytes_remaining: 3,
    };
    budget
        .consume_symbol()
        .expect("the remaining symbol should fit");
    assert!(budget.consume_symbol().is_err());
    budget
        .consume_string("dll", "test strings")
        .expect("the remaining string bytes should fit");
    assert!(budget.consume_string("x", "test strings").is_err());
}

#[test]
fn every_truncated_prefix_is_rejected_without_panicking() {
    let bytes = sample_pe64();
    for length in 0..bytes.len() {
        assert!(
            PeImage::parse(&bytes[..length]).is_err(),
            "truncated PE prefix of {length} bytes was accepted"
        );
    }
}
