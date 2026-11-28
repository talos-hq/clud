use super::*;

/// A non-ELF file is `None`, not a panic or a garbage id.
///
/// This runs in a crash reporter. Anything that can panic while describing a
/// crash is worse than reporting less.
#[test]
fn a_file_that_is_not_elf_is_not_an_error() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = tmp.path().join("not-an-elf");
    std::fs::write(
        &path,
        b"MZ\x90\x00 this is a PE, or at least pretends to be",
    )
    .unwrap();

    assert_eq!(read_elf_build_id(&path), None);
}

/// Truncated and malformed inputs must not panic.
///
/// The parser indexes into whatever bytes are on disk. Every length in an ELF
/// header is attacker-or-corruption controlled from its point of view, so the
/// interesting property is that no input produces anything but `None`.
#[test]
fn malformed_elf_headers_yield_none_rather_than_panicking() {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();

    let cases: &[(&str, Vec<u8>)] = &[
        ("empty", Vec::new()),
        ("magic only", b"\x7fELF".to_vec()),
        // Valid magic, 64-bit, little-endian, then nothing else.
        ("header truncated", {
            let mut v = b"\x7fELF\x02\x01\x01\x00".to_vec();
            v.extend(std::iter::repeat_n(0u8, 8));
            v
        }),
        // Claims a huge phnum with no program headers behind it.
        ("phnum lies", {
            let mut v = vec![0u8; 0x40];
            v[..4].copy_from_slice(b"\x7fELF");
            v[4] = 2; // ELFCLASS64
            v[5] = 1; // little-endian
            v[0x36] = 56; // e_phentsize
            v[0x38] = 0xff; // e_phnum low byte
            v[0x39] = 0xff; // e_phnum high byte
            v
        }),
        // A bad class byte: neither ELFCLASS32 nor ELFCLASS64.
        ("unknown class", {
            let mut v = vec![0u8; 0x40];
            v[..4].copy_from_slice(b"\x7fELF");
            v[4] = 7;
            v[5] = 1;
            v
        }),
    ];

    for (name, bytes) in cases {
        let path = tmp.path().join(name.replace(' ', "-"));
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_elf_build_id(&path), None, "{name} must be None");
    }
}

/// A note section whose records claim zero length must terminate.
///
/// `find_build_id_note` advances by the record's own length fields; a
/// zero-length record would leave the cursor where it was and spin forever.
#[test]
fn a_zero_length_note_record_terminates() {
    // n_namesz = 0, n_descsz = 0, n_type = 0 -> a record that advances 12
    // bytes, then a second that would not advance at all if the guard were
    // removed.
    let notes = vec![0u8; 24];
    assert_eq!(find_build_id_note(&notes), None);
}

/// A note with the right type but a different owner is not a GNU build-id.
#[test]
fn a_non_gnu_note_of_the_same_type_is_ignored() {
    let mut notes = Vec::new();
    notes.extend(4u32.to_le_bytes()); // n_namesz: "AAA\0"
    notes.extend(4u32.to_le_bytes()); // n_descsz
    notes.extend(NT_GNU_BUILD_ID.to_le_bytes());
    notes.extend(b"AAA\0");
    notes.extend([1u8, 2, 3, 4]);

    assert_eq!(find_build_id_note(&notes), None);
}

/// A well-formed GNU note is decoded to lowercase hex.
#[test]
fn a_well_formed_gnu_note_decodes_to_hex() {
    let mut notes = Vec::new();
    notes.extend(4u32.to_le_bytes());
    notes.extend(4u32.to_le_bytes());
    notes.extend(NT_GNU_BUILD_ID.to_le_bytes());
    notes.extend(b"GNU\0");
    notes.extend([0xde, 0xad, 0xbe, 0xef]);

    assert_eq!(find_build_id_note(&notes).as_deref(), Some("deadbeef"));
}
