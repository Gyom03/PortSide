// Generates the app icon procedurally (no binary assets in the repo) and embeds it
// together with the application manifest (per-monitor DPI awareness v2).

use std::{env, fs, path::PathBuf};

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application><supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/></application>
  </compatibility>
  <asmv3:application>
    <asmv3:windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
    </asmv3:windowsSettings>
  </asmv3:application>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ico = out.join("app.ico");
    let manifest = out.join("app.manifest");
    fs::write(&ico, build_ico(&[16, 20, 24, 32, 40, 48, 64])).unwrap();
    fs::write(&manifest, MANIFEST).unwrap();
    let esc = |p: &PathBuf| p.display().to_string().replace('\\', "\\\\");
    let rc = out.join("app.rc");
    fs::write(&rc, format!("1 ICON \"{}\"\n1 24 \"{}\"\n{}", esc(&ico), esc(&manifest), version_info())).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE).manifest_required().unwrap();
}

/// VERSIONINFO resource: the name Task Manager and the file properties show.
fn version_info() -> String {
    let v = env::var("CARGO_PKG_VERSION").unwrap();
    let n: Vec<u16> = v.split(['.', '-']).take(3).map(|p| p.parse().unwrap_or(0)).collect();
    let (a, b, c) = (n[0], n.get(1).copied().unwrap_or(0), n.get(2).copied().unwrap_or(0));
    format!(
        r#"1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "PortSide"
      VALUE "FileDescription", "PortSide"
      VALUE "FileVersion", "{v}"
      VALUE "InternalName", "portside"
      VALUE "OriginalFilename", "portside.exe"
      VALUE "ProductName", "PortSide"
      VALUE "ProductVersion", "{v}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}

// ---- icon: coral rounded square with two white "server rack" bars ----

fn sd_box(p: (f32, f32), c: (f32, f32), h: (f32, f32), r: f32) -> f32 {
    let qx = (p.0 - c.0).abs() - (h.0 - r);
    let qy = (p.1 - c.1).abs() - (h.1 - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn draw(s: usize) -> Vec<[f32; 4]> {
    let sf = s as f32;
    let cov = |d: f32| (0.5 - d * sf).clamp(0.0, 1.0);
    let top = [255.0, 122.0, 92.0];
    let bot = [229.0, 72.0, 77.0];
    let mut px = Vec::with_capacity(s * s);
    for y in 0..s {
        for x in 0..s {
            let p = ((x as f32 + 0.5) / sf, (y as f32 + 0.5) / sf);
            let a_bg = cov(sd_box(p, (0.5, 0.5), (0.47, 0.47), 0.23));
            let t = p.1;
            let bg = [lerp(top[0], bot[0], t), lerp(top[1], bot[1], t), lerp(top[2], bot[2], t)];
            let mut c = bg;
            for cy in [0.335f32, 0.665] {
                let a_bar = cov(sd_box(p, (0.5, cy), (0.285, 0.1), 0.07));
                let mut bar = [255.0f32; 3];
                if s >= 20 {
                    let dx = p.0 - 0.33;
                    let dy = p.1 - cy;
                    let a_dot = cov((dx * dx + dy * dy).sqrt() - 0.042);
                    for i in 0..3 {
                        bar[i] = lerp(bar[i], bot[i], a_dot);
                    }
                }
                for i in 0..3 {
                    c[i] = lerp(c[i], bar[i], a_bar);
                }
            }
            px.push([c[0], c[1], c[2], a_bg * 255.0]);
        }
    }
    px
}

fn build_ico(sizes: &[usize]) -> Vec<u8> {
    let mut images = Vec::new();
    for &s in sizes {
        let px = draw(s);
        let mut d = Vec::new();
        d.extend(40u32.to_le_bytes());
        d.extend((s as i32).to_le_bytes());
        d.extend((2 * s as i32).to_le_bytes());
        d.extend(1u16.to_le_bytes());
        d.extend(32u16.to_le_bytes());
        d.extend([0u8; 24]);
        for y in (0..s).rev() {
            for x in 0..s {
                let [r, g, b, a] = px[y * s + x];
                d.extend([b as u8, g as u8, r as u8, a as u8]);
            }
        }
        let mask_row = s.div_ceil(32) * 4;
        d.extend(vec![0u8; mask_row * s]);
        images.push((s, d));
    }
    let mut out = Vec::new();
    out.extend(0u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend((images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len();
    for (s, d) in &images {
        let dim = if *s >= 256 { 0 } else { *s as u8 };
        out.extend([dim, dim, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((d.len() as u32).to_le_bytes());
        out.extend((offset as u32).to_le_bytes());
        offset += d.len();
    }
    for (_, d) in images {
        out.extend(d);
    }
    out
}
