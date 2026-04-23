import 'package:code_assets/code_assets.dart';
import 'package:hooks/hooks.dart';
import 'package:native_toolchain_rust/native_toolchain_rust.dart';

void main(List<String> args) async {
  await build(args, (input, output) async {
    await RustBuilder(
      assetName: 'src/xlog_bindings.dart',
      cratePath: 'rust',
      features: const <String>['metrics-prometheus'],
      extraCargoEnvironmentVariables: _appleDeploymentTargetEnv(
        input.config.code,
      ),
    ).run(input: input, output: output);
  });
}

// rustc's apple targets default to `-mios-version-min=10.0` /
// `-mmacosx-version-min=10.7`. Without `IPHONEOS_DEPLOYMENT_TARGET` /
// `MACOSX_DEPLOYMENT_TARGET`, the link step is pinned to those defaults
// while the .rlibs are compiled against the installed SDK, so symbols added
// to libSystem in newer iOS/macOS releases (e.g. `___chkstk_darwin` on iOS
// 13+) fail to resolve. native_toolchain_rust 1.0.3 does not propagate
// these env vars, so we forward the deployment target Flutter passes us.
Map<String, String> _appleDeploymentTargetEnv(CodeConfig code) {
  return switch (code.targetOS) {
    OS.iOS => {'IPHONEOS_DEPLOYMENT_TARGET': '${code.iOS.targetVersion}'},
    OS.macOS => {'MACOSX_DEPLOYMENT_TARGET': '${code.macOS.targetVersion}'},
    _ => const <String, String>{},
  };
}
