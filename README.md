# LightningView Image Viewer and Browser
 A lightning-fast cross-platform image viewer written in Rust

This is a very slim image viewer that aims to replicate the most important functions found in commercial software like ACDSee.

## Core Design goals:

* Lightweight
* Cross Platform
* Common image format support
* RAW file support for popular cameras
* Browsing through the current directory with arrow keys
* Pan/Zoom with the mouse
* Basic file operations such as deletion
* Quick way to start the default image editor (planned)

## Non-goals:

* Any form of image modification
* File format conversion
* Plugin system

## Command Line Parameters:

To start viewing images:
```
lightningview.exe <imagefile.ext>
```

To register as valid default program for viewing images on Windows:
```
lightningview.exe /register
```

To remove this registration from your windows registry and settings:
```
lightningview.exe /unregister
```


## Controls:

| Input | Action |
| ----------- | ----------- |
| Left Cursor | Show previous image in directory |
| Right Cursor | Show next image in directory | 
| Home | Jump to first image in directory |
| End | Jump to last image in directory |
| Delete | Delete the currently viewed image file |
| Mouse wheel | Zoom in / out |
| Drag Mouse | Pan image|

## TODO:

* Animated GIF support
