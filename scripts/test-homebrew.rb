require "json"
require "open3"
require "tmpdir"
require "formulary"

def check(condition, message)
  raise message unless condition
end

generator = File.expand_path("homebrew-formula.rb", __dir__)
Dir.mktmpdir("kodrik-homebrew-") do |directory|
  archive = File.join(directory, "kodrik-1.2.3-aarch64-apple-darwin.tar.gz")
  File.write(archive, "abc")
  output, error, status = Open3.capture3(RbConfig.ruby, generator, "v1.2.3", archive)
  check(status.success?, "Formula generation failed: #{error}")
  path = Pathname.new(directory)/"kodrik.rb"
  formula = Formulary.from_contents("kodrik", path, output)
  check(formula.version.to_s == "1.2.3", "Wrong formula version")
  check(formula.stable.url == "https://github.com/suinly/kodrik/releases/download/v1.2.3/kodrik-1.2.3-aarch64-apple-darwin.tar.gz", "Wrong release URL")
  check(formula.stable.checksum.to_s == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "Checksum does not match archive bytes")

  plist, error, status = Open3.capture3("/usr/bin/plutil", "-convert", "json", "-o", "-", "-", stdin_data: formula.service.to_plist)
  check(status.success?, "Invalid service plist: #{error}")
  service = JSON.parse(plist)
  check(service.fetch("Label") == "com.kodrik.gateway", "Service migration changed the launchd label")
  check(service.fetch("ProgramArguments") == ["#{formula.opt_bin}/kodrik", "gateway"], "Service does not use the stable executable path")
  check(service.fetch("KeepAlive") == true, "Cooperative restart needs KeepAlive")
  check(service.fetch("WorkingDirectory") == "#{Dir.home}/.kodrik/workspace", "Service must not work in the Cellar or source checkout")
  check(service.fetch("StandardOutPath") == "#{Dir.home}/.kodrik/logs/kodrik-gateway.log", "Wrong log destination")
  check(service.fetch("EnvironmentVariables").fetch("PATH").split(":").include?("#{HOMEBREW_PREFIX}/bin"), "Homebrew tools are absent from service PATH")

  [[], ["1.2.3", archive], ["v1.2.3-rc.1", archive], ["v1.2.3\nputs :injected", archive], ["v1.2.3", "#{directory}/missing.tar.gz"]].each do |arguments|
    output, _, status = Open3.capture3(RbConfig.ruby, generator, *arguments)
    check(!status.success? && output.empty?, "Invalid input generated a formula: #{arguments.inspect}")
  end
end
puts "Homebrew formula and service checks passed."
