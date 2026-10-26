//! Test fixture: checks exact arguments and writes synthetic output.
use std::io::Write as _;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let expected: Vec<String> = serde_json::from_slice(&std::fs::read("expected.json")?)?;
    if args != expected {
        return Err(format!("Arguments differ: {args:?}, expected {expected:?}").into());
    }
    std::fs::write("started", b"started")?;
    let mode = std::fs::read_to_string("mode").unwrap_or_default();
    match mode.as_str() {
        "sleep" => {
            println!("waiting for slice");
            std::thread::sleep(std::time::Duration::from_secs(3));
            std::fs::write("finished", b"finished")?;
        }
        "fail" => {
            for _ in 0..10000 {
                println!("old stdout");
                eprintln!("old stderr");
            }
            println!("last stdout");
            eprintln!("last stderr");
            std::process::exit(7);
        }
        _ => (),
    }
    use base64::Engine as _;
    let png = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")?;
    if let Some(path) = args.iter().find_map(|s| s.strip_prefix("--output=")) {
        let mut file = std::fs::File::create(path)?;
        if mode == "binary" {
            file.write_all(b"GCDE\0binary")?;
            return Ok(());
        }
        if mode == "missing" {
            writeln!(file, "G1 X0")?;
            return Ok(());
        }
        writeln!(file, "; estimated printing time (normal mode) = 2m 3s\n; filament used [mm] = 450\n; filament used [g] = 1.5\n; total layers count = 12")?;
        writeln!(
            file,
            "; thumbnail begin 1x1 0\n; {}\n; thumbnail end\nG1 X0",
            base64::engine::general_purpose::STANDARD.encode(&png)
        )?;
    } else {
        let path = args
            .iter()
            .find_map(|s| s.strip_prefix("--export-3mf="))
            .ok_or("No export")?;
        let mut zip = zip::ZipWriter::new(std::fs::File::create(Path::new(path))?);
        for (name, bytes) in [
            ("Metadata/plate_1.gcode", b"; model printing time: 1m 50s; total estimated time: 2m 3s\n; total filament length [mm] : 450\n; total filament weight [g] : 1.5\n; total layer number: 12\nG1 X0".as_slice()),
            ("Metadata/plate_1.png", png.as_slice()),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())?;
            zip.write_all(bytes)?;
        }
        zip.finish()?;
    }
    Ok(())
}
