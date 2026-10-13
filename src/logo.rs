use std::{io::Cursor, path::PathBuf};

const PNG: &[u8] = include_bytes!("../data/io.github.gnome_git_release_tray.png");

pub fn cached_path() -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("io", "github", "gnome-git-release-tray")
        .ok_or_else(|| anyhow::anyhow!("Cannot determine icon cache directory"))?;
    let directory = dirs.cache_dir().join("icons");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{}.png", crate::desktop::APP_ID));
    if std::fs::read(&path).ok().as_deref() != Some(PNG) {
        let temporary = path.with_extension("png.tmp");
        std::fs::write(&temporary, PNG)?;
        std::fs::rename(temporary, &path)?;
    }
    Ok(path)
}

pub fn tray_pixmap() -> Vec<ksni::Icon> {
    let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_read(Cursor::new(PNG)) else {
        return vec![];
    };
    let pixels = pixbuf.read_pixel_bytes();
    let mut argb = Vec::with_capacity((pixbuf.width() * pixbuf.height() * 4) as usize);
    for y in 0..pixbuf.height() as usize {
        for x in 0..pixbuf.width() as usize {
            let offset = y * pixbuf.rowstride() as usize + x * pixbuf.n_channels() as usize;
            let alpha = if pixbuf.has_alpha() {
                pixels[offset + 3]
            } else {
                255
            };
            argb.extend_from_slice(&[
                alpha,
                pixels[offset],
                pixels[offset + 1],
                pixels[offset + 2],
            ]);
        }
    }
    vec![ksni::Icon {
        width: pixbuf.width(),
        height: pixbuf.height(),
        data: argb,
    }]
}
