class CocoaWay < Formula
  desc "Native macOS Wayland compositor for running Linux apps"
  homepage "https://github.com/J-x-Z/cocoa-way"
  url "https://github.com/J-x-Z/cocoa-way/archive/refs/tags/v2.1.3.tar.gz"
  sha256 "bbbf8e9f0d4850573335d9a8cc0963f7b578e3236df88e03d0325b77d7ffea69"
  license "GPL-3.0-only"
  head "https://github.com/J-x-Z/cocoa-way.git", branch: "main"

  depends_on "pkg-config" => :build
  depends_on "rust" => :build
  depends_on "libxkbcommon"
  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args
    system "bash", "scripts/bundle-example-resources.sh", share/"cocoa-way/container-images"
  end

  def caveats
    <<~EOS
      Cocoa-Way runs Linux GUI applications and container desktops on macOS.

      Quick start:
        1. Start the compositor:
           cocoa-way

        2. Connect Linux clients via waypipe:
           brew install J-x-Z/tap/waypipe-darwin
           waypipe ssh user@linux-host <program>

      For more info: https://github.com/J-x-Z/cocoa-way
    EOS
  end

  test do
    assert_match "cocoa-way", shell_output("#{bin}/cocoa-way --help 2>&1")
    assert_match "applications", shell_output("#{bin}/cocoa-wayctl --help")
    assert_predicate bin/"cocoa-way-mcp", :executable?
    assert_predicate share/"cocoa-way/container-images/examples/container-images/Containerfile.niri", :file?
    assert_predicate share/"cocoa-way/container-images/examples/container-images/cocoa-way-clipboard-relay", :executable?
  end
end
