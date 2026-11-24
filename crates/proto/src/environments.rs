//! Recommended free 360° environments.
//!
//! hyprvr does not ship these images. `vrd env fetch` (or the Download button in the headset's
//! environment picker) downloads each original from its source and converts it locally into
//! the user's environments folder. The credits travel with the images: `CREDITS.md` in that
//! folder, `vrd env credits`, and a credit line in the headset's picker.

/// What to do with the downloaded original.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Process {
    /// Already a 2:1 equirectangular JPEG; used as is.
    AsIs,
    /// 2:1 equirectangular PNG; re-encoded as JPEG (the headset gets files up to 48 MB).
    Jpeg,
    /// Wide panorama covering the sky down to the horizon: scaled to 8192 px wide, placed at
    /// the top of a 8192x4096 canvas, the missing ground filled with a dark fade.
    PadGround,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recommended {
    /// File name in the environments folder.
    pub file: &'static str,
    pub title: &'static str,
    pub author: &'static str,
    pub license: &'static str,
    /// Page with the original and its license.
    pub source: &'static str,
    /// Direct download of the original.
    pub url: &'static str,
    /// Size of the original in bytes (for progress; not enforced).
    pub bytes: u64,
    pub process: Process,
}

macro_rules! ph {
    ($id:literal, $title:literal, $author:literal, $bytes:literal) => {
        Recommended {
            file: concat!($id, ".jpg"),
            title: $title,
            author: $author,
            license: "CC0",
            source: concat!("https://polyhaven.com/a/", $id),
            url: concat!("https://dl.polyhaven.org/file/ph-assets/HDRIs/extra/Tonemapped%20JPG/", $id, ".jpg"),
            bytes: $bytes,
            process: Process::AsIs,
        }
    };
}

pub const RECOMMENDED: &[Recommended] = &[
    ph!("comfy_cafe", "Comfy Cafe", "Sergej Majboroda / Poly Haven", 3_407_886),
    ph!("lakeside", "Lakeside", "Greg Zaal / Poly Haven", 4_538_493),
    ph!("venice_sunset", "Venice Sunset", "Greg Zaal / Poly Haven", 4_822_313),
    ph!("wooden_lounge", "Wooden Lounge", "Greg Zaal / Poly Haven", 4_557_177),
    ph!("the_sky_is_on_fire", "The Sky is On Fire", "Greg Zaal, Rico Cilliers / Poly Haven", 3_467_321),
    Recommended {
        file: "galaxy_core.jpg",
        title: "Milky Way 360° (galactic core)",
        author: "Kevin M. Loch",
        license: "CC BY-SA 4.0",
        source: "https://commons.wikimedia.org/wiki/File:Milkyway360-latlon-32kscaled8k-4200k-m14-s4-g1.png",
        url: "https://upload.wikimedia.org/wikipedia/commons/3/32/Milkyway360-latlon-32kscaled8k-4200k-m14-s4-g1.png",
        bytes: 76_731_754,
        process: Process::Jpeg,
    },
    Recommended {
        file: "galaxy_deep.jpg",
        title: "Milky Way 360° (foreground stars removed)",
        author: "Kevin M. Loch",
        license: "CC BY-SA 4.0",
        source: "https://commons.wikimedia.org/wiki/File:Milky_Way_360_equirectangular_rendering_with_foreground_stars_removed.png",
        url: "https://upload.wikimedia.org/wikipedia/commons/1/19/Milky_Way_360_equirectangular_rendering_with_foreground_stars_removed.png",
        bytes: 59_731_145,
        process: Process::Jpeg,
    },
    Recommended {
        file: "alma_milky_way.jpg",
        title: "Milky Way at Chajnantor",
        author: "Y. Beletsky (LCO) / ESO",
        license: "CC BY 4.0",
        source: "https://commons.wikimedia.org/wiki/File:Milky_Way_at_Chajnantor_(uhd_9428_panorama_eq).jpg",
        url: "https://upload.wikimedia.org/wikipedia/commons/7/77/Milky_Way_at_Chajnantor_%28uhd_9428_panorama_eq%29.jpg",
        bytes: 20_376_421,
        process: Process::PadGround,
    },
    Recommended {
        file: "atacama_moonlight.jpg",
        title: "Petroglyphs and the galaxy",
        author: "ESO / B. Tafreshi (twanight.org)",
        license: "CC BY 4.0",
        source: "https://commons.wikimedia.org/wiki/File:Petroglyphs_and_the_galaxy_(uhd_img_8535pv2-cc_eq).jpg",
        url: "https://upload.wikimedia.org/wikipedia/commons/c/c5/Petroglyphs_and_the_galaxy_%28uhd_img_8535pv2-cc_eq%29.jpg",
        bytes: 30_194_499,
        process: Process::PadGround,
    },
    Recommended {
        file: "paranal_milky_way.jpg",
        title: "Milky Way glow at Paranal",
        author: "ESO / P. Horálek",
        license: "CC BY 4.0",
        source: "https://commons.wikimedia.org/wiki/File:Milky_Way_glow_at_Paranal_(2016-04-03-paranal-vlt-360fd-not-extended-cc).jpg",
        url: "https://upload.wikimedia.org/wikipedia/commons/c/c1/Milky_Way_glow_at_Paranal_%282016-04-03-paranal-vlt-360fd-not-extended-cc%29.jpg",
        bytes: 45_848_919,
        process: Process::PadGround,
    },
];

/// Catalog entry for an environment file name, if it is one of the recommended ones.
pub fn recommended(file: &str) -> Option<&'static Recommended> {
    RECOMMENDED.iter().find(|r| r.file == file)
}

/// Total download size of the originals.
pub fn total_bytes() -> u64 {
    RECOMMENDED.iter().map(|r| r.bytes).sum()
}

/// One-line credit, e.g. "Lakeside · Greg Zaal / Poly Haven · CC0".
pub fn credit_line(r: &Recommended) -> String {
    format!("{} · {} · {}", r.title, r.author, r.license)
}

/// Contents of `CREDITS.md` for the recommended files present in a folder.
pub fn credits_markdown(present: &dyn Fn(&str) -> bool) -> String {
    let mut s = String::from(
        "# Environment credits\n\n\
         Downloaded by hyprvr (`vrd env fetch`) from the original sources and converted locally\n\
         (scaled / re-encoded, missing ground filled in for the ESO panoramas).\n\
         The licenses apply to these files if you share them.\n\n",
    );
    for r in RECOMMENDED.iter().filter(|r| present(r.file)) {
        s += &format!("- **{}** (`{}`): {}, {}. {}\n", r.title, r.file, r.author, r.license, r.source);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent() {
        let mut files: Vec<_> = RECOMMENDED.iter().map(|r| r.file).collect();
        files.sort();
        files.dedup();
        assert_eq!(files.len(), RECOMMENDED.len());
        for r in RECOMMENDED {
            assert!(r.file.ends_with(".jpg"), "{}", r.file);
            assert!(r.url.starts_with("https://"), "{}", r.url);
            if r.process == Process::AsIs {
                assert!(r.url.starts_with("https://dl.polyhaven.org/"));
            }
        }
        assert!(recommended("lakeside.jpg").is_some());
        assert!(credits_markdown(&|f| f == "lakeside.jpg").contains("Greg Zaal"));
        assert!(total_bytes() > 200_000_000);
    }
}
