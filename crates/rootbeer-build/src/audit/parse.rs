use super::{Binary, Format};
use std::path::Path;

pub(crate) fn is_native(magic: [u8; 4]) -> bool {
    magic == *b"\x7fELF"
        || matches!(
            u32::from_be_bytes(magic),
            0xfeedface
                | 0xcefaedfe
                | 0xfeedfacf
                | 0xcffaedfe
                | 0xcafebabe
                | 0xbebafeca
                | 0xcafebabf
                | 0xbfbafeca
        )
}

pub(super) fn parse(bytes: &[u8], path: &Path) -> Result<Vec<Binary>, String> {
    if bytes.starts_with(b"\x7fELF") {
        let elf = goblin::elf::Elf::parse(bytes).map_err(|error| error.to_string())?;
        if elf.dynamic.as_ref().is_some_and(|dynamic| {
            dynamic.dyns.iter().any(|entry| {
                matches!(
                    entry.d_tag,
                    goblin::elf::dynamic::DT_CONFIG
                        | goblin::elf::dynamic::DT_AUDIT
                        | goblin::elf::dynamic::DT_DEPAUDIT
                        | 0x7fff_fffd
                        | 0x7fff_ffff
                )
            })
        }) {
            return Err("ELF audit/configuration/filter loading is not supported".into());
        }
        if !matches!(
            elf.header.e_type,
            goblin::elf::header::ET_EXEC | goblin::elf::header::ET_DYN
        ) {
            return Ok(Vec::new());
        }
        return Ok(vec![Binary {
            path: path.into(),
            format: Format::Elf,
            architecture: elf.header.e_machine.into(),
            bits: if elf.is_64 { 64 } else { 32 },
            little_endian: elf.little_endian,
            is_executable: elf.interpreter.is_some()
                || elf.header.e_type == goblin::elf::header::ET_EXEC,
            interpreter: elf.interpreter.map(str::to_owned),
            identity: elf.soname.map(str::to_owned),
            libraries: elf.libraries.into_iter().map(str::to_owned).collect(),
            rpaths: elf
                .rpaths
                .into_iter()
                .flat_map(|paths| paths.split(':').map(str::to_owned))
                .collect(),
            runpaths: elf
                .runpaths
                .into_iter()
                .flat_map(|paths| paths.split(':').map(str::to_owned))
                .collect(),
        }]);
    }
    let mut binaries = Vec::new();
    match goblin::mach::Mach::parse(bytes).map_err(|error| error.to_string())? {
        goblin::mach::Mach::Binary(mach) => macho(&mach, bytes, path, &mut binaries)?,
        goblin::mach::Mach::Fat(fat) => {
            for arch in fat.iter_arches() {
                let arch = arch.map_err(|error| error.to_string())?;
                let slice = arch.slice(bytes);
                if slice.starts_with(b"!<arch>\n") {
                    continue;
                }
                let mach =
                    goblin::mach::MachO::parse(slice, 0).map_err(|error| error.to_string())?;
                macho(&mach, slice, path, &mut binaries)?;
            }
        }
    }
    Ok(binaries)
}

fn macho(
    mach: &goblin::mach::MachO<'_>,
    bytes: &[u8],
    path: &Path,
    binaries: &mut Vec<Binary>,
) -> Result<(), String> {
    if !matches!(
        mach.header.filetype,
        goblin::mach::header::MH_EXECUTE
            | goblin::mach::header::MH_DYLIB
            | goblin::mach::header::MH_BUNDLE
    ) {
        return Ok(());
    }
    let mut interpreter = None;
    for command in &mach.load_commands {
        if matches!(
            command.command,
            goblin::mach::load_command::CommandVariant::DyldEnvironment(_)
                | goblin::mach::load_command::CommandVariant::PreboundDylib(_)
        ) {
            return Err("Mach-O environment/prebound loading is not supported".into());
        }
        if let goblin::mach::load_command::CommandVariant::LoadDylinker(loader) = command.command {
            let start = command
                .offset
                .checked_add(loader.name as usize)
                .ok_or("loader offset overflow")?;
            let end = command
                .offset
                .checked_add(loader.cmdsize as usize)
                .ok_or("loader size overflow")?;
            let value = bytes.get(start..end).ok_or("invalid loader string")?;
            let length = value
                .iter()
                .position(|byte| *byte == 0)
                .ok_or("unterminated loader string")?;
            interpreter = Some(
                std::str::from_utf8(&value[..length])
                    .map_err(|error| error.to_string())?
                    .into(),
            );
        }
    }
    binaries.push(Binary {
        path: path.into(),
        format: Format::MachO,
        architecture: mach.header.cputype,
        bits: if mach.is_64 { 64 } else { 32 },
        little_endian: mach.little_endian,
        is_executable: mach.header.filetype == goblin::mach::header::MH_EXECUTE,
        interpreter,
        identity: mach.name.map(str::to_owned),
        libraries: mach
            .libs
            .iter()
            .skip(1)
            .map(|value| (*value).into())
            .collect(),
        rpaths: mach.rpaths.iter().map(|value| (*value).into()).collect(),
        runpaths: Vec::new(),
    });
    Ok(())
}
