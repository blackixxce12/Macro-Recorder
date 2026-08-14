# Icon assets

| File | Used by | Notes |
|---|---|---|
| `icon.ico` | `build.rs` → embedded in the `.exe` | Multi-resolution: 256, 128, 64, 48, 32, 24, 16 |
| `icon.rgba` | `main.rs` → window / taskbar icon | Raw 128×128 RGBA (65 536 bytes), no decoder needed |
| `icon_512.png` | Master artwork | Transparent rounded corners |
| `icon_256.png` | README, GitHub social preview | |
| `icon_128.png` | Docs | |

## Regenerating

If the artwork changes, replace `icon_512.png` (RGBA, transparent corners) and run:

```python
# pip install pillow
from PIL import Image

master = Image.open("icon_512.png").convert("RGBA")

master.save("icon.ico", format="ICO",
            sizes=[(256,256),(128,128),(64,64),(48,48),(32,32),(24,24),(16,16)])
master.resize((256, 256), Image.LANCZOS).save("icon_256.png")
master.resize((128, 128), Image.LANCZOS).save("icon_128.png")

# raw RGBA blob consumed by include_bytes! in main.rs
open("icon.rgba", "wb").write(master.resize((128, 128), Image.LANCZOS).tobytes())
```

`icon.rgba` **must** stay exactly 128 × 128 × 4 = 65 536 bytes — `main.rs` checks the
length and silently falls back to the OS default icon if it doesn't match.
