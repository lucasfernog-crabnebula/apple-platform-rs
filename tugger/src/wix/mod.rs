// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod common;
mod installer_builder;
mod simple_msi_builder;
mod wxs_builder;

pub use common::*;
pub use installer_builder::*;
pub use simple_msi_builder::*;
pub use wxs_builder::*;

use {
    crate::{
        http::{download_to_path, RemoteContent},
        zipfile::extract_zip,
    },
    anyhow::{Context, Result},
    handlebars::Handlebars,
    lazy_static::lazy_static,
    slog::warn,
    std::{
        borrow::Cow,
        collections::BTreeMap,
        io::Write,
        path::{Path, PathBuf},
    },
    uuid::Uuid,
    xml::{
        common::XmlVersion,
        writer::{EmitterConfig, EventWriter, XmlEvent},
    },
};

lazy_static! {
    static ref WIX_TOOLSET: RemoteContent = RemoteContent {
        url: "https://github.com/wixtoolset/wix3/releases/download/wix3112rtm/wix311-binaries.zip"
            .to_string(),
        sha256: "2c1888d5d1dba377fc7fa14444cf556963747ff9a0a289a3599cf09da03b9e2e".to_string(),
    };

    // Latest versions of the VC++ Redistributable can be found at
    // https://support.microsoft.com/en-us/help/2977003/the-latest-supported-visual-c-downloads.
    // The download URL will redirect to a deterministic artifact, which is what we
    // record here.

    static ref VC_REDIST_X86: RemoteContent = RemoteContent {
        url: "https://download.visualstudio.microsoft.com/download/pr/48431a06-59c5-4b63-a102-20b66a521863/CAA38FD474164A38AB47AC1755C8CCCA5CCFACFA9A874F62609E6439924E87EC/VC_redist.x86.exe".to_string(),
        sha256: "caa38fd474164a38ab47ac1755c8ccca5ccfacfa9a874f62609e6439924e87ec".to_string(),
    };

    static ref VC_REDIST_X64: RemoteContent = RemoteContent {
        url: "https://download.visualstudio.microsoft.com/download/pr/48431a06-59c5-4b63-a102-20b66a521863/4B5890EB1AEFDF8DFA3234B5032147EB90F050C5758A80901B201AE969780107/VC_redist.x64.exe".to_string(),
        sha256: "4b5890eb1aefdf8dfa3234b5032147eb90f050c5758a80901b201ae969780107".to_string(),
    };

    static ref VC_REDIST_ARM64: RemoteContent = RemoteContent {
        url: "https://download.visualstudio.microsoft.com/download/pr/48431a06-59c5-4b63-a102-20b66a521863/A950A1C9DB37E2F784ABA98D484A4E0F77E58ED7CB57727672F9DC321015469E/VC_redist.arm64.exe".to_string(),
        sha256: "a950a1c9db37e2f784aba98d484a4e0f77e58ed7cb57727672f9dc321015469e".to_string(),
    };

    static ref HANDLEBARS: Handlebars<'static> = {
        let mut handlebars = Handlebars::new();

        handlebars
            .register_template_string("bundle.wxs", include_str!("templates/bundle.wxs"))
            .unwrap();

        handlebars
    };
}

/// Entity used to build a WiX bundle installer.
///
/// Bundle installers have multiple components in them.
#[derive(Default)]
pub struct WiXBundleInstallerBuilder {
    /// Name of the bundle.
    name: String,

    /// Version of the application.
    version: String,

    /// Manufacturer string.
    manufacturer: String,

    /// UUID upgrade code.
    upgrade_code: Option<String>,

    /// Conditions that must be met to perform the install.
    conditions: Vec<(String, String)>,

    /// Whether to include an x86 Visual C++ Redistributable.
    include_vc_redist_x86: bool,

    /// Whether to include an amd64 Visual C++ Redistributable.
    include_vc_redist_x64: bool,

    /// Whether to include an arm64 Visual C++ Redistributable.
    include_vc_redist_arm64: bool,

    /// Keys to define in the preprocessor when running candle.
    preprocess_parameters: BTreeMap<String, String>,
}

impl WiXBundleInstallerBuilder {
    pub fn new(name: String, version: String, manufacturer: String) -> Self {
        Self {
            name,
            version,
            manufacturer,
            ..Self::default()
        }
    }

    fn upgrade_code(&self) -> Cow<'_, str> {
        if let Some(code) = &self.upgrade_code {
            Cow::Borrowed(code)
        } else {
            Cow::Owned(
                Uuid::new_v5(
                    &Uuid::NAMESPACE_DNS,
                    format!("tugger.bundle.{}", &self.name).as_bytes(),
                )
                .to_string(),
            )
        }
    }

    /// Define a `<bal:Condition>` that must be satisfied to run this installer.
    ///
    /// `message` is the message that will be displayed if the condition is not met.
    /// `condition` is the condition expression. e.g. `VersionNT = v8.0`.
    pub fn add_condition(&mut self, message: &str, condition: &str) {
        self.conditions
            .push((message.to_string(), condition.to_string()));
    }

    /// Add this instance to a `WiXInstallerBuilder`.
    ///
    /// Requisite files will be downloaded and this instance will be converted to
    /// a wxs file and registered with the builder.
    pub fn add_to_installer_builder(
        &self,
        logger: &slog::Logger,
        builder: &mut WiXInstallerBuilder,
    ) -> Result<()> {
        let redist_x86_path = builder.build_path().join("vc_redist.x86.exe");
        let redist_x64_path = builder.build_path().join("vc_redist.x64.exe");
        let redist_arm64_path = builder.build_path().join("vc_redist.arm64.exe");

        if self.include_vc_redist_x86 {
            warn!(logger, "fetching Visual C++ Redistribution (x86)");
            download_to_path(logger, &VC_REDIST_X86, &redist_x86_path)?;
        }

        if self.include_vc_redist_x64 {
            warn!(logger, "fetching Visual C++ Redistributable (x64)");
            download_to_path(logger, &VC_REDIST_X64, &redist_x64_path)?;
        }

        if self.include_vc_redist_arm64 {
            warn!(logger, "fetching Visual C++ Redistribution (arm64)");
            download_to_path(logger, &VC_REDIST_ARM64, &redist_arm64_path)?;
        }

        let mut emitter_config = EmitterConfig::new();
        emitter_config.perform_indent = true;

        let buffer = Vec::new();
        let writer = std::io::BufWriter::new(buffer);
        let mut emitter = emitter_config.create_writer(writer);
        self.write_bundle_xml(&mut emitter)?;

        let mut wxs =
            WxsBuilder::from_data(Path::new("bundle.wxs"), emitter.into_inner().into_inner()?);
        for (k, v) in &self.preprocess_parameters {
            wxs.set_preprocessor_parameter(k, v);
        }

        builder.add_wxs(wxs);

        Ok(())
    }

    fn write_bundle_xml<W: Write>(&self, writer: &mut EventWriter<W>) -> Result<()> {
        writer.write(XmlEvent::StartDocument {
            version: XmlVersion::Version10,
            encoding: Some("utf-8"),
            standalone: None,
        })?;

        writer.write(
            XmlEvent::start_element("Wix")
                .default_ns("http://schemas.microsoft.com/wix/2006/wi")
                .ns("bal", "http://schemas.microsoft.com/wix/BalExtension")
                .ns("util", "http://schemas.microsoft.com/wix/UtilExtension"),
        )?;

        // TODO Condition?
        writer.write(
            XmlEvent::start_element("Bundle")
                .attr("Name", &self.name)
                .attr("Version", &self.version)
                .attr("Manufacturer", &self.manufacturer)
                .attr("UpgradeCode", self.upgrade_code().as_ref()),
        )?;

        writer.write(
            XmlEvent::start_element("BootstrapperApplicationRef")
                .attr("Id", "WixStandardBootstrapperApplication.HyperlinkLicense"),
        )?;

        writer.write(
            XmlEvent::start_element("bal:WixStandardBootstrapperApplication")
                .attr("LicenseUrl", "")
                .attr("SuppressOptionsUI", "yes"),
        )?;
        writer.write(XmlEvent::end_element())?;

        // </BootstrapperApplicationRef>
        writer.write(XmlEvent::end_element())?;

        for (message, condition) in &self.conditions {
            writer.write(XmlEvent::start_element("bal:Condition").attr("Message", message))?;
            writer.write(XmlEvent::CData(condition))?;
            writer.write(XmlEvent::end_element())?;
        }

        writer.write(XmlEvent::start_element("Chain"))?;

        if self.include_vc_redist_x86 {
            writer.write(
                XmlEvent::start_element("ExePackage")
                    .attr("Id", "vc_redist.x86.exe")
                    .attr("Cache", "no")
                    .attr("Compressed", "yes")
                    .attr("PerMachine", "yes")
                    .attr("Permanent", "yes")
                    .attr("InstallCondition", "Not VersionNT64")
                    .attr("InstallCommand", "/install /quiet /norestart")
                    .attr("RepairCommand", "/repair /quiet /norestart")
                    .attr("UninstallCommand", "/uninstall /quiet /norestart"),
            )?;

            // </ExePackage>
            writer.write(XmlEvent::end_element())?;
        }

        if self.include_vc_redist_x64 {
            writer.write(
                XmlEvent::start_element("ExePackage")
                    .attr("Id", "vc_redist.x64.exe")
                    .attr("Cache", "no")
                    .attr("Compressed", "yes")
                    .attr("PerMachine", "yes")
                    .attr("Permanent", "yes")
                    .attr("InstallCondition", "VersionNT64")
                    .attr("InstallCommand", "/install /quiet /norestart")
                    .attr("RepairCommand", "/repair /quiet /norestart")
                    .attr("UninstallCommand", "/uninstall /quiet /norestart"),
            )?;

            // </ExePackage>
            writer.write(XmlEvent::end_element())?;
        }

        if self.include_vc_redist_arm64 {
            writer.write(
                XmlEvent::start_element("ExePackage")
                    .attr("Id", "vc_redist.arm64.exe")
                    .attr("Cache", "no")
                    .attr("Compressed", "yes")
                    .attr("PerMachine", "yes")
                    .attr("Permanent", "yes")
                    // TODO properly detect ARM64 here.
                    .attr("InstallCondition", "VersionNT64")
                    .attr("InstallCommand", "/install /quiet /norestart")
                    .attr("RepairCommand", "/repair /quiet /norestart")
                    .attr("UninstallCommand", "/uninstall /quiet /norestart"),
            )?;

            // </ExePackage>
            writer.write(XmlEvent::end_element())?;
        }

        // </Chain>
        writer.write(XmlEvent::end_element())?;
        // </Bundle>
        writer.write(XmlEvent::end_element())?;
        // </Wix>
        writer.write(XmlEvent::end_element())?;

        Ok(())
    }
}

fn extract_wix<P: AsRef<Path>>(logger: &slog::Logger, dest_dir: P) -> Result<PathBuf> {
    let dest_dir = dest_dir.as_ref();

    if !dest_dir.exists() {
        std::fs::create_dir_all(dest_dir)
            .with_context(|| format!("creating {}", dest_dir.display()))?;
    }

    let zip_path = dest_dir.join(format!("wix-toolset.{}.zip", &WIX_TOOLSET.sha256[0..16]));
    let extract_path = dest_dir.join(format!("wix-toolset.{}", &WIX_TOOLSET.sha256[0..16]));

    if !extract_path.exists() {
        download_to_path(logger, &WIX_TOOLSET, &zip_path)
            .with_context(|| format!("downloading to {}", zip_path.display()))?;
        let fh = std::fs::File::open(&zip_path)?;
        let cursor = std::io::BufReader::new(fh);
        warn!(logger, "extracting WiX...");
        extract_zip(cursor, &extract_path)
            .with_context(|| format!("extracting zip to {}", extract_path.display()))?;
    }

    Ok(extract_path)
}

#[cfg(test)]
mod tests {
    use {super::*, crate::testutil::*};

    #[test]
    fn test_wix_download() -> Result<()> {
        let logger = get_logger()?;

        extract_wix(&logger, DEFAULT_DOWNLOAD_DIR.as_path())?;

        Ok(())
    }

    #[test]
    fn test_vcredist_download() -> Result<()> {
        let logger = get_logger()?;

        download_to_path(
            &logger,
            &VC_REDIST_X86,
            &DEFAULT_DOWNLOAD_DIR.join("vc_redist.x86.exe"),
        )?;
        download_to_path(
            &logger,
            &VC_REDIST_X64,
            &DEFAULT_DOWNLOAD_DIR.join("vc_redist.x64.exe"),
        )?;
        download_to_path(
            &logger,
            &VC_REDIST_ARM64,
            &DEFAULT_DOWNLOAD_DIR.join("vc_redist.arm64.exe"),
        )?;

        Ok(())
    }
}
