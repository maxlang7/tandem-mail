{ pkgs ? import <nixpkgs> {} }:
pkgs.mkShell {
  packages = with pkgs; [ cargo rustc rustfmt clippy pkg-config ];
  TANDEM_MAIL_BOLD_FONT = "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans-Bold.ttf";
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [
    dbus libxkbcommon wayland libGL libglvnd
    libx11 libxcursor libxi libxrandr libxcb
  ]);
}
