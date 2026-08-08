# Arasaka USB Flasher

The Arasaka USB Flasher is the official provisioning utility for the Arasaka
Linux platform. It retrieves the current Arasaka Linux image, reassembles and
verifies it, and writes it to the selected USB drive — providing a secure,
straightforward method for deploying the Arasaka environment to hardware.

## Overview

The application streamlines image provisioning in a single operation:

1. **Select a target drive** from the detected removable devices.
2. **Initiate the flash.** The application downloads the current Arasaka Linux
   image from official Arasaka infrastructure, reassembles all components,
   and validates the image's cryptographic signature.
3. **Provisioning.** Once verification succeeds, the verified image is written
   to the selected drive.

No technical configuration is required. The application handles image
retrieval, integrity verification, and device provisioning automatically.

## Distribution

The Arasaka USB Flasher is distributed through the following official
channels:

| Platform | Distribution |
| --- | --- |
| Flatpak | `flatpak install flathub org.arasaka.usb` |
| Snap | `snap install arasaka-usb` |
| Windows | Standalone installer and portable executable, available from the [Releases](https://github.com/arasaka-cpu/arasaka-usb/releases) page |

### Privacy

The application contacts official Arasaka infrastructure only to retrieve the
current image manifest and its components. No user data is collected or
transmitted.

## License

Copyright © 2026 Arasaka. Licensed under the [MIT License](LICENSE).
