![Logo](https://raw.githubusercontent.com/dividebysandwich/LightningView/main/lightningview.png)

# LightningView Image Viewer and Browser
 A lightning-fast cross-platform image viewer written in Rust

This is a very slim image viewer that aims to replicate the most important functions found in commercial software like ACDSee.

## Core Design goals

* Lightweight
* Cross Platform
* Common image format support
* RAW file support for popular cameras
* Basic FITS support with autostretching
* Video playback with audio and subtitles
* Browsing through the current directory with arrow keys
* Pan/Zoom with the mouse
* Basic file operations such as deletion
* Quick way to start the default image editor (planned)

## Non-goals

* Any form of image modification
* File format conversion
* Plugin system
* Complex histogram based stretching for FITS

## Installation

Download the appropriate package from the [release page](https://github.com/dividebysandwich/LightningView/releases)

Arch Linux users can install by running ```yay -S lightningview``` or ```paru -S lightningview```

## Command Line Parameters

To start viewing images:
```
lightningview <imagefile.ext>
```

To open an image in windowed mode instead of fullscreen:
```
lightningview /windowed <imagefile.ext>
```

To register as default program for viewing images on older versions of Windows:
```
lightningview.exe /register
```

To remove this registration from your windows registry and settings:
```
lightningview.exe /unregister
```


## Controls

| Input | Action |
| ----------- | ----------- |
| Left Cursor | Show previous image in directory |
| Right Cursor | Show next image in directory | 
| Home | Jump to first image in directory |
| End | Jump to last image in directory |
| R | Sort images randomly |
| N | Sort images by name |
| F | Toggle fullscreen |
| Enter | Toggle between scale to fit and 1:1 display |
| Delete | Delete the currently viewed image file |
| Ctrl+C | Copy current image to clipboard |
| Mouse wheel | Zoom in / out |
| Drag Mouse | Pan image|

## Video playback

Video files are detected by their extension and handed to an ffmpeg-backed player
that runs alongside the image viewer. Playback includes synchronized audio (the
audio track drives the A/V clock; videos without audio fall back to a wall clock)
and subtitle rendering. A seek/progress bar is shown transiently on screen when
seeking or toggling playback.

While a video is playing, the cursor keys seek within the file instead of
navigating the directory; use Page Up / Page Down to move to the previous / next
file.

### Video controls

| Input | Action |
| ----------- | ----------- |
| Space | Toggle play / pause |
| Left Cursor | Seek backward 5 seconds |
| Right Cursor | Seek forward 5 seconds |
| Ctrl+Left Cursor | Seek backward 60 seconds |
| Ctrl+Right Cursor | Seek forward 60 seconds |
| Page Up | Show previous file in directory |
| Page Down | Show next file in directory |
| A | Cycle through audio tracks |
| S | Cycle through subtitle tracks |
| F | Toggle fullscreen |

## Supported image formats

General image formats:

* BMP
* GIF
* ICO
* JPEG
* JPEG-XL
* PNG
* PNM
* SVG
* TIFF
* TGA
* WEBP
* XBM
* XPM

RAW formats:

* Minolta MRW
* Sony ARW, SRF and SR2
* Mamiya MEF
* Olympus ORF
* Samsung SRW
* Epson ERF
* Kodak KDC
* Kodak DCS
* Panasonic / Leica RW2
* Fuji RAF
* Kodak DCR
* Adobe DNG
* Pentax PEF
* Canon CRW
* Leaf / Phase One IIQ
* Hasselblad 3FR
* Nikon NRW
* Nikon NEF
* Leaf MOS
* Canon CR2
* ARRI's ARI
* Astrophotography FITS (experimental)

## Supported video formats

* MP4
* MKV
* WEBM
* AVI
* MOV
* M4V
* MPG / MPEG
* WMV
* FLV
* TS

(Decoding is handled by ffmpeg, so most common audio/video codecs within these
containers are supported.)

## TODO / Feature Requests

* Add a way to edit the currently viewed file
* Improved FITS handling
* Display sorting mode on screen when pressing R or N
* OpenCL support for RAW processing

## Compiling

Just run the usual command:

```
cargo build --release
```

Under Linux, you may need to install additional dependencies first:

```
apt install libx11-dev libcairo-dev libxcursor-dev libxfixes-dev libxinerama-dev libxft-dev libpango1.0-dev libstdc++-11-dev
```

Video playback is built on ffmpeg, so the ffmpeg development libraries are also
required:

```
apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libswresample-dev
```

## Star History

<a href="https://www.star-history.com/#dividebysandwich/LightningView&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/svg?repos=dividebysandwich/LightningView&type=date&theme=dark&legend=top-left" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/svg?repos=dividebysandwich/LightningView&type=date&legend=top-left" />
   <img alt="Star History Chart" src="https://api.star-history.com/svg?repos=dividebysandwich/LightningView&type=date&legend=top-left" />
 </picture>
</a>


