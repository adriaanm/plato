//! A/B the G16 dithers on a real image, from the command line:
//!
//! ```sh
//! cargo run -p plato-core --release --example dither_ab photo.png out-dir
//! ```
//!
//! Writes three PNGs to `out-dir`: the input quantized with no dither at all,
//! and with each of the two error diffusions -- everything downstream of the
//! same grayscale conversion, so the differences on screen are the algorithms
//! and nothing else. A 0..255 gradient strip is appended below the photo,
//! because smooth ramps are where 16 levels band first. (The framebuffer's
//! blue-noise mask is not here: its `resources/blue_noise-128.png` is not in
//! this fork's tree.)

use plato_core::framebuffer::{Framebuffer, Pixmap};
use plato_core::framebuffer::dither::{dither_g16_atkinson, dither_g16_floyd_steinberg,
                                      dither_g16_stucki};

const GRADIENT_ROWS: u32 = 96;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(out_dir)) = (args.next(), args.next()) else {
        eprintln!("usage: dither_ab <input.png> <out-dir>");
        std::process::exit(2);
    };

    let file = std::io::BufReader::new(std::fs::File::open(&input).expect("readable input"));
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().expect("a PNG");
    let mut buf = vec![0; reader.output_buffer_size().expect("sane dimensions")];
    let info = reader.next_frame(&mut buf).expect("decodable PNG");
    let (width, height) = (info.width, info.height);
    let samples = info.color_type.samples();

    // Luma via Rec. 601 weights on the code values -- the same rough cut the
    // rest of the codebase makes (Color::gray).
    let mut pixmap = Pixmap::new(width, height + GRADIENT_ROWS, 1);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let at = (y * width as usize + x) * samples;
            let gray = match samples {
                1 | 2 => buf[at],
                _ => ((buf[at] as u32 * 77 + buf[at + 1] as u32 * 151 +
                       buf[at + 2] as u32 * 28) >> 8) as u8,
            };
            pixmap.data[y * width as usize + x] = gray;
        }
    }
    for y in 0..GRADIENT_ROWS as usize {
        for x in 0..width as usize {
            let value = (x * 255 / (width as usize - 1)) as u8;
            pixmap.data[(height as usize + y) * width as usize + x] = value;
        }
    }

    std::fs::create_dir_all(&out_dir).expect("out dir");
    let save = |name: &str, pixmap: &Pixmap| {
        let path = format!("{out_dir}/{name}.png");
        pixmap.save(&path).expect("written PNG");
        println!("{path}");
    };

    let mut nearest = pixmap.clone();
    for value in nearest.data.iter_mut() {
        *value = ((*value as u32 + 8) / 17 * 17).min(255) as u8;
    }
    save("a-nearest", &nearest);

    let mut floyd = pixmap.clone();
    dither_g16_floyd_steinberg(&mut floyd);
    save("c-floyd-steinberg", &floyd);

    let mut stucki = pixmap.clone();
    dither_g16_stucki(&mut stucki);
    save("d-stucki", &stucki);

    let mut atkinson = pixmap;
    dither_g16_atkinson(&mut atkinson);
    save("e-atkinson", &atkinson);
}
