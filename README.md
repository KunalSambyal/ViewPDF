# ViewPDF

A lightweight, fast, no-nonsense PDF viewer for Windows. No ads, no subscriptions, no background bloat—just open a PDF and read.

> **Note:** This project was vibecoded with AI to solve a personal annoyance with heavy browsers and bloated PDF readers.

## Download & Run (No Coding Required)

1. Go to the **[Releases](../../releases)** page on the right.
2. Download the latest `.zip` file.
3. Extract the folder anywhere on your PC (make sure `ViewPDF.exe` and `pdfium.dll` stay together in the same folder).
4. Run `ViewPDF.exe` and drag-and-drop any PDF onto the window (or set it as your default PDF app in Windows).

## Features

- **Lightweight & Fast:** Uses a fraction of the RAM compared to Chrome/Edge or Adobe Acrobat.
- **Zero Bloat:** No ads, no popups, and uses 0% CPU when sitting idle.
- **Smooth Controls:**
    - **Drag & Drop** any `.pdf` file into the window to open it
    - **`Ctrl + Wheel` or `+` / `-`** to zoom in and out (`0` to reset zoom)

## Building from Source

If you want to tweak the code or add your own features:

1. Install [Rust](https://rust-lang.org).
2. Clone this repository and run:
    ```bash
    cargo build --release
    ```
3. Download `pdfium.dll` (Windows x64) from [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries/releases) and place it next to the built `.exe` in `target/release/`.
