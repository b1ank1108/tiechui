use std::fs;
use std::path::Path;

use crate::Result;

pub fn run(root: &Path, check: bool) -> Result<()> {
    let mut generator = schemars::r#gen::SchemaGenerator::default();
    generator.subschema_for::<tiechui_protocol::ProgramRun>();
    let protocol = generator.into_root_schema_for::<tiechui_protocol::ProtocolMessage>();
    for (relative, schema) in [
        ("schema/protocol.json", protocol),
        ("schema/config.json", tiechui_config::schema()),
        ("schema/providers.json", tiechui_config::providers_schema()),
        (
            "schema/plugin.json",
            schemars::schema_for!(tiechui_protocol::PluginProtocol),
        ),
    ] {
        let generated = format!("{}\n", serde_json::to_string_pretty(&schema)?);
        let path = root.join(relative);
        if check {
            let current = fs::read_to_string(&path).map_err(|error| {
                format!(
                    "{}: {error}; run cargo run -p xtask -- schema",
                    path.display()
                )
            })?;
            if current != generated {
                return Err(
                    format!("{relative} is stale; run cargo run -p xtask -- schema").into(),
                );
            }
        } else {
            fs::create_dir_all(root.join("schema"))?;
            fs::write(&path, generated)?;
            println!("Wrote {}", path.display());
        }
    }
    if check {
        println!("Protocol and configuration schemas are current");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_rejects_missing_and_stale_schema_without_writing() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/schema-tests");
        fs::create_dir_all(&directory).unwrap();
        let temp = tempfile::tempdir_in(directory).unwrap();
        let paths = [
            temp.path().join("schema/protocol.json"),
            temp.path().join("schema/config.json"),
            temp.path().join("schema/providers.json"),
            temp.path().join("schema/plugin.json"),
        ];
        assert!(run(temp.path(), true).is_err());
        assert!(paths.iter().all(|path| !path.exists()));
        run(temp.path(), false).unwrap();
        let generated: Vec<_> = paths
            .iter()
            .map(|path| fs::read_to_string(path).unwrap())
            .collect();
        run(temp.path(), true).unwrap();
        for path in &paths {
            fs::write(path, "{}\n").unwrap();
            let error = run(temp.path(), true).unwrap_err().to_string();
            assert!(error.contains("stale"));
            assert!(error.contains(path.file_name().unwrap().to_str().unwrap()));
            assert_eq!(fs::read_to_string(path).unwrap(), "{}\n");
            run(temp.path(), false).unwrap();
            for (path, expected) in paths.iter().zip(&generated) {
                assert_eq!(&fs::read_to_string(path).unwrap(), expected);
            }
        }
        fs::remove_file(&paths[1]).unwrap();
        assert!(
            run(temp.path(), true)
                .unwrap_err()
                .to_string()
                .contains("schema/config.json")
        );
        assert!(!paths[1].exists());
    }
}
