use std::{collections::HashSet, fs, io::Result, path::PathBuf};

use prost::Message;
use prost_types::FileDescriptorSet;

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=liqi_config/liqi.desc");

    let descriptor_bytes = fs::read("liqi_config/liqi.desc")?;
    let mut descriptor_set = FileDescriptorSet::decode(descriptor_bytes.as_slice())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    // The modder only uses lq protocol types. The other packages describe
    // legacy resource tables that max_data.yaml replaces.
    descriptor_set
        .file
        .retain(|file| file.package.as_deref() == Some("lq"));

    // Only probe responses whose field 1 is actually lq.Error. A blanket decoder
    // can mistake ordinary nested messages for errors (e.g. common views).
    let error_responses: HashSet<_> = descriptor_set
        .file
        .iter()
        .flat_map(|file| &file.message_type)
        .filter(|message| {
            message.field.iter().any(|field| {
                field.number == Some(1) && field.type_name.as_deref() == Some(".lq.Error")
            })
        })
        .map(|message| format!(".lq.{}", message.name()))
        .collect();
    let mut error_methods = Vec::new();
    for file in &descriptor_set.file {
        for service in &file.service {
            for method in &service.method {
                if error_responses.contains(method.output_type()) {
                    error_methods.push(format!(".lq.{}.{}", service.name(), method.name()));
                }
            }
        }
    }
    error_methods.sort();
    error_methods.dedup();
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    fs::write(
        out_dir.join("error_response_methods.rs"),
        format!("const ERROR_RESPONSE_METHODS: &[&str] = &{error_methods:?};\n"),
    )?;

    let mut config = prost_build::Config::new();
    config
        .type_attribute(".", "#[allow(dead_code)]")
        .type_attribute(
            "lq.ViewSlot",
            "#[derive(::serde::Serialize, ::serde::Deserialize)]",
        );
    // 输出到 OUT_DIR 而非 src/proto，避免生成代码污染 cargo fmt / clippy 的检查范围
    config.compile_fds(descriptor_set)?;

    Ok(())
}
