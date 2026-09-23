// Must be its own module, and the FIRST import in main.jsx.
//
// ES modules evaluate the entire import graph before any statement in the
// importing module's body, so doing this inline in main.jsx assigns Buffer
// *after* @coral-xyz/anchor and @solana/web3.js have already been evaluated —
// which is the "Buffer is not defined" this fixes. As a separate module it
// evaluates in import order, i.e. before them.
import { Buffer } from "buffer";

globalThis.Buffer ??= Buffer;
