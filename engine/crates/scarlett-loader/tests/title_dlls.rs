//! A title DLL that cannot be loaded is named in the error; one that is no
//! x64 image is not the title's and is passed over.
#![cfg(windows)]

#[test]
fn a_title_dll_that_does_not_prepare_is_named() {
    let title = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(title.path().join("broken.dll"), b"MZ not a PE").unwrap();
    let wanted = |_: &str| true;
    let loaded = scarlett_loader::load_title_dll("broken.dll", title.path(), cache.path(), &wanted);
    let Err(error) = loaded else {
        panic!("a DLL that is not a PE loaded");
    };
    let text = format!("{error:#}");
    assert!(text.contains("broken.dll"), "{text}");
}

/// Just the headers `load_title_dll` reads first: DOS header, signature,
/// file header (machine) and the optional header's magic.
fn headers(machine: u16, magic: u16) -> Vec<u8> {
    let mut pe = vec![0u8; 0x100];
    pe[..2].copy_from_slice(b"MZ");
    pe[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    pe[0x80..0x84].copy_from_slice(b"PE\0\0");
    pe[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
    pe[0x98..0x9a].copy_from_slice(&magic.to_le_bytes());
    pe
}

/// A 32-bit or ARM64 DLL beside the title is not the title's (#508: a copy
/// from SysWOW64): passed over, not an error that stops the run.
#[test]
fn a_dll_for_another_processor_is_passed_over() {
    let title = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let wanted = |_: &str| true;
    for (name, machine, magic) in [("x86.dll", 0x014c, 0x10b), ("arm.dll", 0xaa64, 0x20b)] {
        std::fs::write(title.path().join(name), headers(machine, magic)).unwrap();
        let loaded = scarlett_loader::load_title_dll(name, title.path(), cache.path(), &wanted);
        assert!(loaded.unwrap().is_empty(), "{name} was mapped");
    }
    // A real one, where Windows has it.
    let windir = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let x86 = std::path::Path::new(&windir).join(r"SysWOW64\xinput1_4.dll");
    if x86.is_file() {
        std::fs::copy(&x86, title.path().join("xinput1_4.dll")).unwrap();
        let loaded =
            scarlett_loader::load_title_dll("XINPUT1_4.dll", title.path(), cache.path(), &wanted);
        assert!(
            loaded.unwrap().is_empty(),
            "the SysWOW64 xinput1_4.dll was mapped"
        );
    }
}

/// An x64 header with nothing behind it is still prepared, and fails as a
/// damaged file does: only a whole PE of another kind is passed over.
#[test]
fn a_damaged_x64_dll_is_still_an_error() {
    let title = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(title.path().join("cut.dll"), headers(0x8664, 0x20b)).unwrap();
    let wanted = |_: &str| true;
    let loaded = scarlett_loader::load_title_dll("cut.dll", title.path(), cache.path(), &wanted);
    assert!(loaded.is_err(), "a cut-off x64 DLL loaded");
}
