project(
  'duck-packages',
  'rust',
  version: '0.1.0',
  license: 'GPL-3.0-or-later',
  meson_version: '>= 1.3.0',
)

gnome = import('gnome')
i18n = import('i18n')

app_id = 'io.github.srwalkerb.DuckPackages'
profile = get_option('profile')
cargo_profile = profile == 'release' ? 'release' : 'dev'
cargo_target = profile == 'release' ? 'release' : 'debug'

dependency('gtk4', version: '>= 4.14')
dependency('libadwaita-1', version: '>= 1.5')
dependency('appstream', version: '>= 1.0')

subdir('data')
subdir('po')
subdir('src')

summary({
  'Version': meson.project_version(),
  'Profile': profile,
  'Application ID': app_id,
}, section: 'Duck Packages')

