// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::{LswError, Result};

const DOS_HEADER_BYTES: usize = 64;
const COFF_HEADER_BYTES: usize = 20;
const SECTION_HEADER_BYTES: usize = 40;
const IMPORT_DESCRIPTOR_BYTES: usize = 20;
const COR20_HEADER_BYTES: usize = 72;
const MAX_PE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_IMPORT_DESCRIPTORS: usize = 4096;
const MAX_IMPORTS_PER_DLL: usize = 65_536;
const MAX_IMPORT_NAME_BYTES: usize = 4096;
const MAX_TOTAL_IMPORT_SYMBOLS: usize = 65_536;
const MAX_TOTAL_IMPORT_STRING_BYTES: usize = 16 * 1024 * 1024;

const IMAGE_FILE_DLL: u16 = 0x2000;
const IMAGE_DIRECTORY_ENTRY_IMPORT: usize = 1;
const IMAGE_DIRECTORY_ENTRY_SECURITY: usize = 4;
const IMAGE_DIRECTORY_ENTRY_COM_DESCRIPTOR: usize = 14;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeKind {
    Pe32,
    Pe32Plus,
}

impl fmt::Display for PeKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Pe32 => "PE32",
            Self::Pe32Plus => "PE32+",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeMachine {
    X86,
    X86_64,
    Arm,
    Thumb,
    ArmNt,
    Arm64,
    Unknown(u16),
}

impl PeMachine {
    fn from_raw(value: u16) -> Self {
        match value {
            0x014c => Self::X86,
            0x8664 => Self::X86_64,
            0x01c0 => Self::Arm,
            0x01c2 => Self::Thumb,
            0x01c4 => Self::ArmNt,
            0xaa64 => Self::Arm64,
            unknown => Self::Unknown(unknown),
        }
    }

    pub fn raw(self) -> u16 {
        match self {
            Self::X86 => 0x014c,
            Self::X86_64 => 0x8664,
            Self::Arm => 0x01c0,
            Self::Thumb => 0x01c2,
            Self::ArmNt => 0x01c4,
            Self::Arm64 => 0xaa64,
            Self::Unknown(value) => value,
        }
    }
}

impl fmt::Display for PeMachine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::X86 => formatter.write_str("x86"),
            Self::X86_64 => formatter.write_str("x86_64"),
            Self::Arm => formatter.write_str("Arm"),
            Self::Thumb => formatter.write_str("Thumb"),
            Self::ArmNt => formatter.write_str("Arm NT"),
            Self::Arm64 => formatter.write_str("Arm64"),
            Self::Unknown(value) => write!(formatter, "unknown (0x{value:04x})"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeSubsystem {
    Unknown,
    Native,
    WindowsGui,
    WindowsConsole,
    PosixConsole,
    EfiApplication,
    EfiBootServiceDriver,
    EfiRuntimeDriver,
    Xbox,
    WindowsBootApplication,
    Other(u16),
}

impl PeSubsystem {
    fn from_raw(value: u16) -> Self {
        match value {
            0 => Self::Unknown,
            1 => Self::Native,
            2 => Self::WindowsGui,
            3 => Self::WindowsConsole,
            7 => Self::PosixConsole,
            10 => Self::EfiApplication,
            11 => Self::EfiBootServiceDriver,
            12 => Self::EfiRuntimeDriver,
            14 => Self::Xbox,
            16 => Self::WindowsBootApplication,
            unknown => Self::Other(unknown),
        }
    }

    pub fn raw(self) -> u16 {
        match self {
            Self::Unknown => 0,
            Self::Native => 1,
            Self::WindowsGui => 2,
            Self::WindowsConsole => 3,
            Self::PosixConsole => 7,
            Self::EfiApplication => 10,
            Self::EfiBootServiceDriver => 11,
            Self::EfiRuntimeDriver => 12,
            Self::Xbox => 14,
            Self::WindowsBootApplication => 16,
            Self::Other(value) => value,
        }
    }
}

impl fmt::Display for PeSubsystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => formatter.write_str("unknown"),
            Self::Native => formatter.write_str("native/driver"),
            Self::WindowsGui => formatter.write_str("Windows GUI"),
            Self::WindowsConsole => formatter.write_str("Windows console"),
            Self::PosixConsole => formatter.write_str("POSIX console"),
            Self::EfiApplication => formatter.write_str("EFI application"),
            Self::EfiBootServiceDriver => formatter.write_str("EFI boot-service driver"),
            Self::EfiRuntimeDriver => formatter.write_str("EFI runtime driver"),
            Self::Xbox => formatter.write_str("Xbox"),
            Self::WindowsBootApplication => formatter.write_str("Windows boot application"),
            Self::Other(value) => write!(formatter, "other ({value})"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeSection {
    pub name: String,
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub raw_offset: u32,
    pub raw_size: u32,
    pub characteristics: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PeImportSymbol {
    Name { hint: u16, name: String },
    Ordinal(u16),
}

impl fmt::Display for PeImportSymbol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name { name, .. } => formatter.write_str(name),
            Self::Ordinal(value) => write!(formatter, "#{value}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeImport {
    pub dll: String,
    pub symbols: Vec<PeImportSymbol>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeSupportLevel {
    Supported,
    Conditional,
    Unsupported,
}

impl fmt::Display for PeSupportLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Supported => "supported",
            Self::Conditional => "conditional",
            Self::Unsupported => "unsupported",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeAssessment {
    pub level: PeSupportLevel,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeImage {
    pub kind: PeKind,
    pub machine: PeMachine,
    pub subsystem: PeSubsystem,
    pub timestamp: u32,
    pub characteristics: u16,
    pub entry_point_rva: u32,
    pub image_base: u64,
    pub size_of_image: u32,
    pub is_dll: bool,
    pub is_managed: bool,
    /// Whether the PE declares a structurally in-bounds certificate table.
    /// This does not cryptographically verify an Authenticode signature.
    pub has_certificate_table: bool,
    pub sections: Vec<PeSection>,
    pub imports: Vec<PeImport>,
}

impl PeImage {
    pub fn read(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_PE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PE_BYTES {
            return Err(invalid(format!(
                "{} is larger than the {} MiB inspection limit",
                path.display(),
                MAX_PE_BYTES / 1024 / 1024
            )));
        }
        Self::parse(&bytes)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        require(bytes, 0, DOS_HEADER_BYTES, "DOS header")?;
        if &bytes[0..2] != b"MZ" {
            return Err(invalid("missing DOS MZ signature"));
        }

        let pe_offset = usize::try_from(read_u32(bytes, 0x3c, "PE header offset")?)
            .map_err(|_| invalid("PE header offset does not fit this host"))?;
        require(bytes, pe_offset, 4 + COFF_HEADER_BYTES, "PE/COFF header")?;
        if &bytes[pe_offset..pe_offset + 4] != b"PE\0\0" {
            return Err(invalid("missing PE signature"));
        }

        let coff = checked_add(pe_offset, 4, "COFF header offset")?;
        let machine = PeMachine::from_raw(read_u16(bytes, coff, "COFF machine")?);
        let number_of_sections = usize::from(read_u16(bytes, coff + 2, "section count")?);
        if number_of_sections > 96 {
            return Err(invalid(format!(
                "unreasonable section count {number_of_sections}"
            )));
        }
        let timestamp = read_u32(bytes, coff + 4, "COFF timestamp")?;
        let optional_size = usize::from(read_u16(bytes, coff + 16, "optional header size")?);
        let characteristics = read_u16(bytes, coff + 18, "COFF characteristics")?;
        let optional = checked_add(coff, COFF_HEADER_BYTES, "optional header offset")?;
        require(bytes, optional, optional_size, "optional header")?;

        let magic = read_u16(bytes, optional, "optional header magic")?;
        let (kind, data_directory_offset, directory_count_offset, image_base) = match magic {
            0x010b => {
                require_optional(optional_size, 96, "PE32 optional header")?;
                (
                    PeKind::Pe32,
                    96usize,
                    92usize,
                    u64::from(read_u32(bytes, optional + 28, "PE32 image base")?),
                )
            }
            0x020b => {
                require_optional(optional_size, 112, "PE32+ optional header")?;
                (
                    PeKind::Pe32Plus,
                    112usize,
                    108usize,
                    read_u64(bytes, optional + 24, "PE32+ image base")?,
                )
            }
            unknown => {
                return Err(invalid(format!(
                    "unsupported optional-header magic 0x{unknown:04x}"
                )))
            }
        };
        if matches!(
            (machine, kind),
            (
                PeMachine::X86 | PeMachine::Arm | PeMachine::Thumb | PeMachine::ArmNt,
                PeKind::Pe32Plus,
            ) | (PeMachine::X86_64 | PeMachine::Arm64, PeKind::Pe32)
        ) {
            return Err(invalid(format!(
                "machine {machine} is inconsistent with {kind}"
            )));
        }

        let entry_point_rva = read_u32(bytes, optional + 16, "entry point")?;
        let size_of_image = read_u32(bytes, optional + 56, "image size")?;
        let size_of_headers = read_u32(bytes, optional + 60, "header size")?;
        let subsystem = PeSubsystem::from_raw(read_u16(bytes, optional + 68, "subsystem")?);
        let directory_count = usize::try_from(read_u32(
            bytes,
            optional + directory_count_offset,
            "data-directory count",
        )?)
        .map_err(|_| invalid("data-directory count does not fit this host"))?
        .min(16);
        let directories = parse_directories(
            bytes,
            optional,
            optional_size,
            data_directory_offset,
            directory_count,
        )?;

        let sections_offset = checked_add(optional, optional_size, "section table offset")?;
        let section_table_bytes = number_of_sections
            .checked_mul(SECTION_HEADER_BYTES)
            .ok_or_else(|| invalid("section table size overflow"))?;
        require(bytes, sections_offset, section_table_bytes, "section table")?;
        let mut sections = Vec::with_capacity(number_of_sections);
        for index in 0..number_of_sections {
            let offset = sections_offset + index * SECTION_HEADER_BYTES;
            let name_end = bytes[offset..offset + 8]
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(8);
            let name = String::from_utf8_lossy(&bytes[offset..offset + name_end]).into_owned();
            sections.push(PeSection {
                name,
                virtual_size: read_u32(bytes, offset + 8, "section virtual size")?,
                virtual_address: read_u32(bytes, offset + 12, "section virtual address")?,
                raw_size: read_u32(bytes, offset + 16, "section raw size")?,
                raw_offset: read_u32(bytes, offset + 20, "section raw offset")?,
                characteristics: read_u32(bytes, offset + 36, "section characteristics")?,
            });
        }

        validate_section_ranges(bytes, &sections)?;
        let layout = PeLayout {
            bytes,
            size_of_headers,
            sections: &sections,
        };
        let imports = match directories.get(IMAGE_DIRECTORY_ENTRY_IMPORT) {
            Some(directory) if directory.rva != 0 => parse_imports(&layout, kind, *directory)?,
            _ => Vec::new(),
        };
        let is_managed = match directories.get(IMAGE_DIRECTORY_ENTRY_COM_DESCRIPTOR) {
            Some(directory) if directory.rva != 0 && directory.size != 0 => {
                let length = usize::try_from(directory.size)
                    .map_err(|_| invalid("CLR header size does not fit this host"))?;
                if length < COR20_HEADER_BYTES {
                    return Err(invalid(format!(
                        "CLR header is {length} bytes; at least {COR20_HEADER_BYTES} are required"
                    )));
                }
                layout.rva_to_offset(directory.rva, length, "CLR header")?;
                true
            }
            _ => false,
        };
        let has_certificate_table = match directories.get(IMAGE_DIRECTORY_ENTRY_SECURITY) {
            Some(directory) if directory.rva != 0 && directory.size != 0 => {
                let offset = usize::try_from(directory.rva)
                    .map_err(|_| invalid("certificate file offset does not fit this host"))?;
                let length = usize::try_from(directory.size)
                    .map_err(|_| invalid("certificate size does not fit this host"))?;
                require(bytes, offset, length, "certificate table")?;
                true
            }
            _ => false,
        };

        Ok(Self {
            kind,
            machine,
            subsystem,
            timestamp,
            characteristics,
            entry_point_rva,
            image_base,
            size_of_image,
            is_dll: characteristics & IMAGE_FILE_DLL != 0,
            is_managed,
            has_certificate_table,
            sections,
            imports,
        })
    }

    pub fn imported_symbol_count(&self) -> usize {
        self.imports.iter().map(|import| import.symbols.len()).sum()
    }

    pub fn assess_for_beta(&self) -> PeAssessment {
        let mut level = PeSupportLevel::Supported;
        let mut notes = Vec::new();

        match self.machine {
            PeMachine::X86_64 => {}
            PeMachine::X86 => {
                level = PeSupportLevel::Conditional;
                notes.push(
                    "x86 relies on the Windows x64 guest's WoW64 components; official LSW profiles preserve them"
                        .to_owned(),
                );
            }
            PeMachine::Arm | PeMachine::Thumb | PeMachine::ArmNt | PeMachine::Arm64 => {
                level = PeSupportLevel::Unsupported;
                notes.push("the beta guest is Windows 11 x64, not Windows on Arm".to_owned());
            }
            PeMachine::Unknown(value) => {
                level = PeSupportLevel::Unsupported;
                notes.push(format!(
                    "machine 0x{value:04x} is not recognized by this LSW build"
                ));
            }
        }

        match self.subsystem {
            PeSubsystem::WindowsGui => {
                level = level.max(PeSupportLevel::Conditional);
                notes.push(
                    "the program can run in the guest, but per-window Linux desktop integration is not in this beta"
                        .to_owned(),
                );
            }
            PeSubsystem::Native
            | PeSubsystem::PosixConsole
            | PeSubsystem::EfiApplication
            | PeSubsystem::EfiBootServiceDriver
            | PeSubsystem::EfiRuntimeDriver
            | PeSubsystem::WindowsBootApplication
            | PeSubsystem::Xbox => {
                level = PeSupportLevel::Unsupported;
                notes.push(
                    "this subsystem is not a normal user-mode Windows application".to_owned(),
                );
            }
            PeSubsystem::Unknown | PeSubsystem::Other(_) => {
                level = PeSupportLevel::Unsupported;
                notes.push(
                    "the PE subsystem is not recognized as a supported Windows 11 user-mode application"
                        .to_owned(),
                );
            }
            PeSubsystem::WindowsConsole => {}
        }

        if self.is_dll {
            level = level.max(PeSupportLevel::Conditional);
            notes.push("this is a DLL and must be loaded by a guest process".to_owned());
        }
        if self.is_managed {
            level = level.max(PeSupportLevel::Conditional);
            notes.push(
                "managed assemblies require their matching .NET runtime in the Windows guest unless self-contained"
                    .to_owned(),
            );
        }
        if notes.is_empty() {
            notes.push(
                "the image shape is compatible with the Windows 11 x64 beta guest".to_owned(),
            );
        }

        PeAssessment { level, notes }
    }
}

impl Ord for PeSupportLevel {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for PeSupportLevel {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PeSupportLevel {
    fn rank(self) -> u8 {
        match self {
            Self::Supported => 0,
            Self::Conditional => 1,
            Self::Unsupported => 2,
        }
    }
}

#[derive(Clone, Copy)]
struct DataDirectory {
    rva: u32,
    size: u32,
}

struct PeLayout<'a> {
    bytes: &'a [u8],
    size_of_headers: u32,
    sections: &'a [PeSection],
}

impl PeLayout<'_> {
    fn rva_to_offset(&self, rva: u32, length: usize, what: &str) -> Result<usize> {
        let (offset, available) = self.rva_span(rva, what)?;
        if length > available {
            return Err(invalid(format!("truncated {what}")));
        }
        Ok(offset)
    }

    fn rva_span(&self, rva: u32, what: &str) -> Result<(usize, usize)> {
        if rva < self.size_of_headers {
            let offset = usize::try_from(rva)
                .map_err(|_| invalid(format!("{what} RVA does not fit this host")))?;
            require(self.bytes, offset, 1, what)?;
            let declared = usize::try_from(self.size_of_headers - rva)
                .map_err(|_| invalid(format!("{what} header span does not fit this host")))?;
            return Ok((offset, declared.min(self.bytes.len() - offset)));
        }

        for section in self.sections {
            let span = section.virtual_size.max(section.raw_size);
            let Some(relative) = rva.checked_sub(section.virtual_address) else {
                continue;
            };
            if relative >= span {
                continue;
            }
            if relative >= section.raw_size {
                return Err(invalid(format!(
                    "{what} RVA 0x{rva:08x} points into non-file-backed section data"
                )));
            }
            let offset = section
                .raw_offset
                .checked_add(relative)
                .ok_or_else(|| invalid(format!("{what} file offset overflow")))?;
            let offset = usize::try_from(offset)
                .map_err(|_| invalid(format!("{what} file offset does not fit this host")))?;
            require(self.bytes, offset, 1, what)?;
            let declared = usize::try_from(section.raw_size - relative)
                .map_err(|_| invalid(format!("{what} section span does not fit this host")))?;
            return Ok((offset, declared.min(self.bytes.len() - offset)));
        }
        Err(invalid(format!(
            "{what} RVA 0x{rva:08x} does not map to a section"
        )))
    }
}

fn parse_directories(
    bytes: &[u8],
    optional: usize,
    optional_size: usize,
    directory_offset: usize,
    count: usize,
) -> Result<Vec<DataDirectory>> {
    let bytes_needed = count
        .checked_mul(8)
        .ok_or_else(|| invalid("data-directory table size overflow"))?;
    if directory_offset
        .checked_add(bytes_needed)
        .map_or(true, |end| end > optional_size)
    {
        return Err(invalid("data-directory table exceeds optional header"));
    }
    let mut directories = Vec::with_capacity(count);
    for index in 0..count {
        let offset = optional + directory_offset + index * 8;
        directories.push(DataDirectory {
            rva: read_u32(bytes, offset, "data-directory RVA")?,
            size: read_u32(bytes, offset + 4, "data-directory size")?,
        });
    }
    Ok(directories)
}

fn parse_imports(
    layout: &PeLayout<'_>,
    kind: PeKind,
    directory: DataDirectory,
) -> Result<Vec<PeImport>> {
    let descriptor_limit = if directory.size == 0 {
        MAX_IMPORT_DESCRIPTORS
    } else {
        let declared = usize::try_from(directory.size)
            .map_err(|_| invalid("import-directory size does not fit this host"))?;
        if declared < IMPORT_DESCRIPTOR_BYTES {
            return Err(invalid("import directory is smaller than one descriptor"));
        }
        (declared / IMPORT_DESCRIPTOR_BYTES).min(MAX_IMPORT_DESCRIPTORS)
    };
    let mut imports = Vec::new();
    let mut terminated = false;
    let mut budget = ImportBudget::new();

    for index in 0..descriptor_limit {
        let delta = u32::try_from(index * IMPORT_DESCRIPTOR_BYTES)
            .map_err(|_| invalid("import descriptor RVA overflow"))?;
        let descriptor_rva = directory
            .rva
            .checked_add(delta)
            .ok_or_else(|| invalid("import descriptor RVA overflow"))?;
        let offset =
            layout.rva_to_offset(descriptor_rva, IMPORT_DESCRIPTOR_BYTES, "import descriptor")?;
        let original_first_thunk = read_u32(layout.bytes, offset, "original first thunk")?;
        let timestamp = read_u32(layout.bytes, offset + 4, "import timestamp")?;
        let forwarder_chain = read_u32(layout.bytes, offset + 8, "forwarder chain")?;
        let name_rva = read_u32(layout.bytes, offset + 12, "import DLL name")?;
        let first_thunk = read_u32(layout.bytes, offset + 16, "first thunk")?;
        if original_first_thunk == 0
            && timestamp == 0
            && forwarder_chain == 0
            && name_rva == 0
            && first_thunk == 0
        {
            terminated = true;
            break;
        }
        if name_rva == 0 {
            return Err(invalid("import descriptor has no DLL name"));
        }
        let dll = read_rva_c_string(layout, name_rva, "import DLL name")?;
        budget.consume_string(&dll, "import DLL names")?;
        let thunk_rva = if original_first_thunk != 0 {
            original_first_thunk
        } else {
            first_thunk
        };
        let symbols = if thunk_rva == 0 {
            Vec::new()
        } else {
            parse_import_thunks(layout, kind, thunk_rva, &mut budget)?
        };
        imports.push(PeImport { dll, symbols });
    }

    if !terminated {
        return Err(invalid(format!(
            "import table has no terminator within {descriptor_limit} descriptors"
        )));
    }
    Ok(imports)
}

fn parse_import_thunks(
    layout: &PeLayout<'_>,
    kind: PeKind,
    table_rva: u32,
    budget: &mut ImportBudget,
) -> Result<Vec<PeImportSymbol>> {
    let (width, ordinal_flag) = match kind {
        PeKind::Pe32 => (4usize, 0x8000_0000u64),
        PeKind::Pe32Plus => (8usize, 0x8000_0000_0000_0000u64),
    };
    let mut symbols = Vec::new();
    for index in 0..MAX_IMPORTS_PER_DLL {
        let delta =
            u32::try_from(index * width).map_err(|_| invalid("import thunk RVA overflow"))?;
        let rva = table_rva
            .checked_add(delta)
            .ok_or_else(|| invalid("import thunk RVA overflow"))?;
        let offset = layout.rva_to_offset(rva, width, "import thunk")?;
        let value = if width == 4 {
            u64::from(read_u32(layout.bytes, offset, "import thunk")?)
        } else {
            read_u64(layout.bytes, offset, "import thunk")?
        };
        if value == 0 {
            return Ok(symbols);
        }
        budget.consume_symbol()?;
        if value & ordinal_flag != 0 {
            symbols.push(PeImportSymbol::Ordinal((value & 0xffff) as u16));
            continue;
        }
        let name_rva =
            u32::try_from(value).map_err(|_| invalid("import-by-name RVA exceeds 32 bits"))?;
        let name_offset = layout.rva_to_offset(name_rva, 2, "import-by-name hint")?;
        let hint = read_u16(layout.bytes, name_offset, "import hint")?;
        let string_rva = name_rva
            .checked_add(2)
            .ok_or_else(|| invalid("import name RVA overflow"))?;
        let name = read_rva_c_string(layout, string_rva, "import symbol name")?;
        budget.consume_string(&name, "import symbol names")?;
        symbols.push(PeImportSymbol::Name { hint, name });
    }
    Err(invalid(format!(
        "import thunk table exceeds {MAX_IMPORTS_PER_DLL} entries"
    )))
}

struct ImportBudget {
    symbols_remaining: usize,
    string_bytes_remaining: usize,
}

impl ImportBudget {
    fn new() -> Self {
        Self {
            symbols_remaining: MAX_TOTAL_IMPORT_SYMBOLS,
            string_bytes_remaining: MAX_TOTAL_IMPORT_STRING_BYTES,
        }
    }

    fn consume_symbol(&mut self) -> Result<()> {
        self.symbols_remaining = self.symbols_remaining.checked_sub(1).ok_or_else(|| {
            invalid(format!(
                "import table exceeds the {MAX_TOTAL_IMPORT_SYMBOLS} symbol inspection limit"
            ))
        })?;
        Ok(())
    }

    fn consume_string(&mut self, value: &str, what: &str) -> Result<()> {
        self.string_bytes_remaining = self
            .string_bytes_remaining
            .checked_sub(value.len())
            .ok_or_else(|| {
                invalid(format!(
                    "{what} exceed the {MAX_TOTAL_IMPORT_STRING_BYTES} byte inspection limit"
                ))
            })?;
        Ok(())
    }
}

fn read_rva_c_string(layout: &PeLayout<'_>, rva: u32, what: &str) -> Result<String> {
    let (offset, available_length) = layout.rva_span(rva, what)?;
    let available = &layout.bytes[offset..offset + available_length];
    let length = available
        .iter()
        .take(MAX_IMPORT_NAME_BYTES)
        .position(|byte| *byte == 0)
        .ok_or_else(|| invalid(format!("{what} is not NUL-terminated")))?;
    let value = std::str::from_utf8(&available[..length])
        .map_err(|_| invalid(format!("{what} is not UTF-8/ASCII")))?;
    if value.is_empty() {
        return Err(invalid(format!("{what} is empty")));
    }
    Ok(value.to_owned())
}

fn validate_section_ranges(bytes: &[u8], sections: &[PeSection]) -> Result<()> {
    for section in sections {
        if section.raw_size == 0 {
            continue;
        }
        let offset = usize::try_from(section.raw_offset)
            .map_err(|_| invalid("section file offset does not fit this host"))?;
        let length = usize::try_from(section.raw_size)
            .map_err(|_| invalid("section size does not fit this host"))?;
        require(
            bytes,
            offset,
            length,
            &format!("section {:?}", section.name),
        )?;
    }
    Ok(())
}

fn require_optional(actual: usize, minimum: usize, what: &str) -> Result<()> {
    if actual < minimum {
        return Err(invalid(format!(
            "{what} is {actual} bytes; at least {minimum} are required"
        )));
    }
    Ok(())
}

fn require(bytes: &[u8], offset: usize, length: usize, what: &str) -> Result<()> {
    if offset
        .checked_add(length)
        .map_or(true, |end| end > bytes.len())
    {
        return Err(invalid(format!("truncated {what}")));
    }
    Ok(())
}

fn checked_add(left: usize, right: usize, what: &str) -> Result<usize> {
    left.checked_add(right)
        .ok_or_else(|| invalid(format!("{what} overflow")))
}

fn read_u16(bytes: &[u8], offset: usize, what: &str) -> Result<u16> {
    require(bytes, offset, 2, what)?;
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn read_u32(bytes: &[u8], offset: usize, what: &str) -> Result<u32> {
    require(bytes, offset, 4, what)?;
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn read_u64(bytes: &[u8], offset: usize, what: &str) -> Result<u64> {
    require(bytes, offset, 8, what)?;
    Ok(u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ]))
}

fn invalid(reason: impl Into<String>) -> LswError {
    LswError::InvalidValue {
        field: "PE image",
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
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
}
