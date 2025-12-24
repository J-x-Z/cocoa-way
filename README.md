# Cocoa-Way
<meta name="msvalidate.01" content="F2CE1613C439C59C4D742AC8049C1B5C" />

**Cocoa-Way** is a minimal yet functional Wayland Compositor designed specifically for **macOS**.  
Built with [Rust](https://www.rust-lang.org/) and [Smithay](https://github.com/Smithay/smithay), it allows you to run Linux Wayland applications (like Niri, Sway, or generic clients) seamlessly on your Mac desktop without a virtual machine's GUI overhead.


## ✨ Features

*   **Native macOS Backend**: Seamless integration with macOS desktop environment.
*   **External Client Support**: Host Linux Wayland applications via socket connection.
*   **HiDPI Scaling**: Optimized for Retina displays.
*   **Hardware Acceleration**: Efficient OpenGL rendering pipeline.
*   **Polished Visuals**: server-side decorations with shadows and focus indicators.

## 📚 Research

This project is part of the **"Turbo-Charged Protocol Virtualization"** research initiative. See the [paper folder](../paper/) for:
- Full manuscript draft
- SIMD benchmark harness
- Architecture diagrams

**Key Innovation**: Zero-Cost cross-platform Wayland via Rust trait monomorphization + SIMD-accelerated pixel conversion.

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
./run_waypipe.sh ssh user@linux-host <Program Name>
```

### Example: Local Test Client

```bash
# In the cocoa-way directory, check test-client folder
cargo run --bin test-client
```

## 📄 License

**Copyright © 2025. All Rights Reserved.**

No license is granted for the use, modification, or distribution of this software. You may view the source code for educational purposes, but you may not use it in any commercial or non-commercial projects without explicit written permission from the author.