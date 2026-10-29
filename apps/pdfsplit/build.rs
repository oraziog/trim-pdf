//! Le risorse di Windows dei due eseguibili: l'icona e le proprietà che si leggono col tasto destro
//! del mouse — File, Dettagli.
//!
//! Ogni eseguibile ha la sua icona, quindi il resource compiler gira una volta per binario
//! (`compile_for`) invece di una sola volta per tutto il pacchetto: `pdfsplit.exe` prende il cerchio
//! con il segno della console, `pdfsplit-gui.exe` la finestra con il triangolo verde.
//!
//! Solo su Windows: su altre piattaforme il file non fa nulla e la compilazione non cambia.

fn main() {
    println!("cargo:rerun-if-changed=assets/icona-cli.ico");
    println!("cargo:rerun-if-changed=assets/icona-gui.ico");

    embed();
}

/// Le icone e le proprietà del file, dove il formato esiste davvero.
#[cfg(windows)]
fn embed() {
    // Le icone stanno in `assets/` alla radice del workspace, due livelli sopra questo pacchetto.
    // Se le immagini non sono state ancora generate meglio un avviso che un errore: così una
    // compilazione da un sorgente senza `assets/` continua a girare.
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    for (binary, icon, description) in [
        ("pdfsplit", "../../assets/icona-cli.ico", "pdfsplit - divide PDF molto grandi"),
        ("pdfsplit-gui", "../../assets/icona-gui.ico", "pdfsplit - finestra"),
    ] {
        let icon = root.join(icon);
        let icon = icon.to_string_lossy().into_owned();
        if !std::path::Path::new(&icon).exists() {
            println!("cargo:warning={icon} assente: {binary} resta senza icona");
            continue;
        }

        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(&icon);
        resource.set("FileDescription", description);
        resource.set("ProductName", "pdfsplit");
        resource.set("LegalCopyright", "MIT OR Apache-2.0");

        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let mut numbers = version.split('.').map(|part| part.parse::<u64>().unwrap_or(0));
        let packed = [numbers.next(), numbers.next(), numbers.next()].into_iter().flatten().fold(0_u64, |packed, part| (packed << 16) | part);
        resource.set_version_info(winresource::VersionInfo::FILEVERSION, packed);
        resource.set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
        resource.set_version_info(winresource::VersionInfo::FILEOS, 0x4_0004);
        resource.set_version_info(winresource::VersionInfo::FILETYPE, 1);

        if let Err(error) = resource.compile_for(binary) {
            // Senza resource compiler la CLI si compila lo stesso: è un'icona, non del codice.
            println!("cargo:warning=risorse di {binary} non compilate: {error}");
        }
    }
}

/// Altrove non c'è niente da incidere.
#[cfg(not(windows))]
fn embed() {}
