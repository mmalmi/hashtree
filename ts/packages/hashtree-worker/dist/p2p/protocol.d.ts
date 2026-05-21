import type { WebRTCCommand as CoreWebRTCCommand, WebRTCEvent as CoreWebRTCEvent } from '@hashtree/core';
export type WebRTCCommand = CoreWebRTCCommand;
export type WebRTCEvent = CoreWebRTCEvent | {
    type: 'rtc:bufferHigh';
    peerId: string;
} | {
    type: 'rtc:bufferLow';
    peerId: string;
};
//# sourceMappingURL=protocol.d.ts.map