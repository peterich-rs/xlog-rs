cask "mars-xlog" do
  arch arm: "aarch64-apple-darwin", intel: "x86_64-apple-darwin"

  version "0.1.0"
  sha256 :no_check

  url "https://github.com/peterich-rs/xlog-rs/releases/download/v#{version}/mars-xlog-v#{version}-#{arch}.tar.gz"
  name "mars-xlog"
  desc "Command-line decoder for Tencent Mars xlog files"
  homepage "https://github.com/peterich-rs/xlog-rs"

  binary "mars-xlog"
end
