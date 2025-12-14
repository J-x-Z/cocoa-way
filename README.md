# Cocoa-Way

**Cocoa-Way** is a minimal yet functional Wayland Compositor designed specifically for **macOS**.  
Built with [Rust](https://www.rust-lang.org/) and [Smithay](https://github.com/Smithay/smithay), it allows you to run Linux Wayland applications (like Niri, Sway, or generic clients) seamlessly on your Mac desktop without a virtual machine's GUI overhead.


## ✨ Features

*   **Native macOS Backend**: Seamless integration with macOS desktop environment.
*   **External Client Support**: Host Linux Wayland applications via socket connection.
*   **HiDPI Scaling**: Optimized for Retina displays.
*   **Hardware Acceleration**: Efficient OpenGL rendering pipeline.
*   **Polished Visuals**: server-side decorations with shadows and focus indicators.

## 🚀 Getting Started

### Prerequisites

*   **macOS** 
*   **Rust Toolchain** (latest stable)

### Building

```bash
git clone https://github.com/your-username/cocoa-way.git
cd cocoa-way
cargo build --release
```

### Running

Start the compositor:

```bash
cargo run --release
```

Wait for the "Wayland socket created" message.

## 🔌 Connecting Clients

Use the included helper script to connect clients via SSH or local socket.

### Example: SSH Remote Client

```bash
./run_waypipe.sh ssh user@linux-host niri
```

### Example: Local Test Client

```bash
# In the cocoa-way directory, check test-client folder
cargo run --bin test-client
```

## 📄 License

**Copyright © 2025. All Rights Reserved.**

This project is part of an academic research paper.  
Unauthorised copying, modification, distribution, or use of this code, in whole or in part, is strictly prohibited to prevent plagiarism and preserve academic integrity.
