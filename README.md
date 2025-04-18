![Logo](https://raw.githubusercontent.com/dividebysandwich/LightningView/main/lightningview.png)

# LightningView Image Viewer and Browser
 A lightning-fast cross-platform image viewer written in Rust

This is a very slim image viewer that aims to replicate the most important functions found in commercial software like ACDSee.

## Core Design goals

* Lightweight
* Cross Platform
* Common image format support
* RAW file support for popular cameras
* Browsing through the current directory with arrow keys
* Pan/Zoom with the mouse
* Basic file operations such as deletion
* Quick way to start the default image editor (planned)

## Non-goals

* Any form of image modification
* File format conversion
* Plugin system

## Command Line Parameters

To start viewing images:
```
lightningview.exe <imagefile.ext>
```

To open an image in windowed mode instead of fullscreen:
```
lightningview.exe /windowed <imagefile.ext>
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

## Supported image formats

General image formats:

* BMP
* GIF
* ICO
* JPEG
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

## TODO / Feature Requests

* Add a way to edit the currently viewed file
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

