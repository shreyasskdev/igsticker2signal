# igsticker2signal

Pulls stickers out of Instagram's cache on a rooted Android phone and converts them into Signal-ready PNG/APNG files (512x512, under 300 KB).

Runs in Termux with a live progress UI.

## Requirements

- Rooted Android device (the `su` popup must be approved)
- [Termux](https://termux.dev) with storage access: `termux-setup-storage`
- Rust toolchain: `pkg install rust`

## Install

```
git clone https://github.com/shreyasskdev/igsticker2signal.git
cd igsticker2signal
cargo build --release
```

## Usage

```
./target/release/igsticker2signal
```

Approve the root prompt when it appears. It runs before the UI starts.

| Flag | Effect |
|------|--------|
| `--no-root` | Skip the root copy and only convert originals you already saved |
| `--redo` | Reconvert everything, overwriting existing output |

Press `q` or `Esc` to quit, `Ctrl+C` to abort.

## What it does

1. **Root copy**: with `su`, copies the `.png` and `.webp` files from `/data/data/com.instagram.android/cache/images.stash/clean` into a staging folder and gives Termux ownership of them.
2. **Save originals**: merges them into `originals/`, named by content hash, so reruns never create duplicates.
3. **Static stickers**: fits each image into a 512x512 transparent canvas and saves a PNG. If it's over 300 KB, it retries with stronger compression, then shrinks the sticker and reduces colors until it fits.
4. **Animated stickers**: animated WebPs become APNGs. Animations are capped at 3 seconds. To meet the 300 KB limit it lowers resolution, drops frames, and reduces colors, and as a last resort trims the animation shorter.

Conversion runs in parallel across all CPU cores.

## Output

Everything goes to `Internal storage/signal_stickers/`:

```
signal_stickers/
├── originals/   # deduplicated source files
├── static/      # converted static PNGs
├── animated/    # converted APNGs
└── failed.txt   # files that couldn't be converted (only if any failed)
```

Rerunning only converts new stickers. Use `--redo` to force a full reconversion.

## Using the stickers in Signal

Signal Desktop can create a sticker pack from the PNG/APNG files in `static/` and `animated/`. Signal's limits are 512x512 pixels and 300 KB per sticker.

## Notes

- Root is required because Instagram's cache lives in its private app directory.
- Only stickers Instagram has already cached on your device are found. Open or view the stickers in Instagram first.
- Only use stickers you have the right to use.
