Fixed

- Electrum replies to malformed JSON request lines with `-32700 parse error` and `id: null`, keeping the connection usable for subsequent requests.
