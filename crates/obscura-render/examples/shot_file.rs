//! Render a local HTML file to PNG. Diagnostic aid for isolating whether a bad
//! screenshot comes from Obscura's DOM serialization or from layout/paint.
use obscura_render::{render_html, RenderOptions};
#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let html = std::fs::read_to_string(&args[1]).unwrap();
    let base = args.get(2).cloned();
    let out = args.get(3).cloned().unwrap_or_else(|| "out.png".into());
    let o = RenderOptions {
        width: 1024,
        height: 768,
        ..Default::default()
    };
    let r = render_html(
        &html,
        base.as_deref(),
        &o,
        std::time::Duration::from_secs(10),
    )
    .expect("render");
    println!(
        "{}x{} bytes={} notes={:?}",
        r.width,
        r.height,
        r.bytes.len(),
        r.fidelity_notes
    );
    std::fs::write(&out, &r.bytes).unwrap();
    println!("wrote {out}");
}
