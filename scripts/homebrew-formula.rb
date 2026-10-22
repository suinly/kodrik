require "digest"

tag, archive = ARGV
abort "Usage: ruby scripts/homebrew-formula.rb vX.Y.Z ARCHIVE" unless ARGV.length == 2 && /\Av\d+\.\d+\.\d+\z/.match?(tag)
abort "Archive does not exist: #{archive}" unless File.file?(archive)

version = tag.delete_prefix("v")
checksum = Digest::SHA256.file(archive).hexdigest
puts <<~FORMULA
  class Kodrik < Formula
    desc "Personal AI companion for terminal, Telegram and browser"
    homepage "https://github.com/suinly/kodrik"
    url "https://github.com/suinly/kodrik/releases/download/#{tag}/kodrik-#{version}-aarch64-apple-darwin.tar.gz"
    version "#{version}"
    sha256 "#{checksum}"

    depends_on arch: :arm64
    depends_on macos: :sonoma

    def install
      bin.install "kodrik"
    end

    def caveats
      <<~EOS
        Create ~/.kodrik/config.toml before starting Kodrik.
        Configuration: https://github.com/suinly/kodrik/blob/main/docs/configuration.md

        Run `kodrik` for the terminal UI. After configuring Telegram and/or web,
        run `brew services start suinly/tap/kodrik` for the background gateway.
        Its working directory is ~/.kodrik/workspace; logs are in ~/.kodrik/logs.
        Stop the source-installed com.kodrik.gateway LaunchAgent before migrating.

        This release is ad-hoc signed, without Apple notarization.
        Uninstalling preserves ~/.kodrik, including credentials and history.
      EOS
    end

    service do
      name macos: "com.kodrik.gateway"
      run [opt_bin/"kodrik", "gateway"]
      keep_alive true
      throttle_interval 30
      working_dir File.join(Dir.home, ".kodrik/workspace")
      log_path File.join(Dir.home, ".kodrik/logs/kodrik-gateway.log")
      error_log_path File.join(Dir.home, ".kodrik/logs/kodrik-gateway.err.log")
      environment_variables PATH:     "\#{Dir.home}/.local/bin:\#{Dir.home}/.cargo/bin:\#{std_service_path_env}",
                            RUST_LOG: "info"
    end

    test do
      assert_match "kodrik #{version}", shell_output("\#{bin}/kodrik --version")
      result = JSON.parse(shell_output("\#{bin}/kodrik invites list"))
      assert_empty result.fetch("invites")
      assert_empty result.fetch("users")
    end
  end
FORMULA
