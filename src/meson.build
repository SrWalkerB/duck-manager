cargo_env = environment()
cargo_env.set('DUCK_PACKAGES_LOCALEDIR', get_option('prefix') / get_option('localedir'))
cargo_target_dir = meson.project_build_root() / 'target'
cargo_manifest = meson.project_source_root() / 'Cargo.toml'

cargo_build = custom_target(
  'duck-packages-binary',
  build_by_default: true,
  build_always_stale: true,
  output: 'duck-packages',
  command: [
    find_program('sh'), '-c',
    'CARGO_TARGET_DIR="$2" cargo build --manifest-path "$3" --profile ' + cargo_profile + ' && ' +
    'cp "$2/' + cargo_target + '/duck-packages" "$1"',
    'sh', '@OUTPUT@', cargo_target_dir, cargo_manifest,
  ],
  env: cargo_env,
  install: true,
  install_dir: get_option('bindir'),
)
