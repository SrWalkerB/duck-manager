Name:           duck-packages
Version:        0.1.0
Release:        1%{?dist}
Summary:        View and safely remove installed applications
License:        GPL-3.0-or-later
URL:            https://github.com/SrWalkerB/duck-manager
Source0:        %{name}-%{version}.tar.xz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  meson >= 1.3
BuildRequires:  ninja-build
BuildRequires:  gtk4-devel >= 4.14
BuildRequires:  libadwaita-devel >= 1.5
BuildRequires:  appstream-devel >= 1.0
BuildRequires:  gettext-devel
BuildRequires:  desktop-file-utils
Requires:       PackageKit >= 1.3.5

%description
Duck Packages is a native GNOME utility for understanding and removing
applications installed by the Linux distribution through PackageKit and Polkit.

%prep
%autosetup

%build
export CARGO_NET_OFFLINE=true
meson setup build --prefix=%{_prefix} --buildtype=release -Dprofile=release
meson compile -C build

%install
DESTDIR=%{buildroot} meson install -C build
%find_lang duck-packages

desktop-file-validate %{buildroot}%{_datadir}/applications/io.github.srwalkerb.DuckPackages.desktop
appstreamcli validate --no-net %{buildroot}%{_metainfodir}/io.github.srwalkerb.DuckPackages.metainfo.xml

%files -f duck-packages.lang
%license LICENSE
%doc README.md DESIGN.md docs/SPEC.md
%{_bindir}/duck-packages
%{_datadir}/applications/io.github.srwalkerb.DuckPackages.desktop
%{_metainfodir}/io.github.srwalkerb.DuckPackages.metainfo.xml
%{_datadir}/icons/hicolor/scalable/apps/io.github.srwalkerb.DuckPackages.svg
%{_datadir}/icons/hicolor/symbolic/apps/io.github.srwalkerb.DuckPackages-symbolic.svg

%changelog
* Wed Aug 12 2026 Duck Packages contributors <noreply@example.com> - 0.1.0-1
- Initial development release

