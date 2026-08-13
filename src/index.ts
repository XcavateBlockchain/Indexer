// Polyfill TextEncoder and TextDecoder for the sandbox environment, needed by @solana/kit
import { TextDecoder, TextEncoder } from "text-encoding";
global.TextEncoder = TextEncoder;
global.TextDecoder = TextDecoder;

// Exports all handler functions
export * from "./mappings/mappingHandlers";
