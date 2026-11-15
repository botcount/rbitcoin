Security

- **A forged Tor control reply can no longer plant an onion key or address.**
  The node checks each `ADD_ONION` reply before it saves or advertises
  anything. The `ServiceID` must be a valid v3 onion address. A `PrivateKey`
  must be `ED25519-V3:` followed by Base64 of 64 bytes. A saved
  `onion/*.priv` key gets the same check before it goes into `ADD_ONION`, so
  a key with a space or a line break cannot add arguments or a second
  control command. A bad saved key stops the start with an error that names
  the file to remove. Port of bitcoin/bitcoin#36142 (#1475).
