# LightningView
 A lightning-fast cross-platform image viewer written in Rust

This is a very slim image viewer that aims to replicate the most important functions found in commercial software like ACDSee.

##Core Design goals:

* Lightweight
* Cross Platform
* Common image format support
* RAW file support for popular cameras
* Browsing through the current directory with arrow keys
* Pan/Zoom with the mouse
* Basic file operations such as deletion (planned)
* Quick way to start the default image editor (planned)

##Command Line Parameters:

```
lightningview.exe <imagefile.ext>
```

##Controls:

| Input | Action |
| ----------- | ----------- |
| Left Cursor | Show previous image in directory |
| Right Cursor | Show next image in directory | 
| Mouse wheel | Zoom in / out |
| Drag Mouse | Pan image|


