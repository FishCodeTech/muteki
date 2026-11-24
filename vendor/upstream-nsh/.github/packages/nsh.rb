# Keep the distribution together: nsh finds its runtimes next to bin/nsh.
class Nsh < Formula
  desc "Native UI from TSX modules, with a CLI and prebuilt runtimes"
  homepage "https://nsh.dev"
  # A default URL lets unsupported architectures reach the requirements check.
  url "https://github.com/${NSH_REPO}/releases/download/v${NSH_VERSION}/nsh-macos-arm64.tar.gz"
  sha256 "${NSH_MACOS_ARM64}"
  license "Apache-2.0"

  on_macos do
    depends_on arch: :arm64
  end

  on_linux do
    depends_on arch: :x86_64

    on_intel do
      url "https://github.com/${NSH_REPO}/releases/download/v${NSH_VERSION}/nsh-linux-x86_64.tar.gz"
      sha256 "${NSH_LINUX_X86_64}"
    end
  end

  def install
    libexec.install Dir["*"]
    bin.install_symlink libexec/"bin/nsh"
  end

  test do
    assert_match "nsh #{version}", shell_output("#{bin}/nsh --version")
    system bin/"nsh", "init", "hello"
    system bin/"nsh", "package", "hello", "--target", "web"
    assert_path_exists testpath/".nsh/out/hello/index.html"
    refute_empty (testpath/".nsh/out/hello").glob("nsh.*.wasm")
    refute_empty (testpath/".nsh/out/hello").glob("nsh.*.js")
  end
end
