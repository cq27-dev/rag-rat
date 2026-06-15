export function openDatabase(): void {
  // Open the on-device database connection through the native bridge.
  NativeHeldCore.openDatabase();
}

export type BridgeState = "open" | "closed";

export const bridgeName = "held-mini";

export interface BridgeConfig {
  readonly name: string;
}

export class BridgeClient {
  open(): void {}
}

export const useBridge = (): string => {
  const currentBridgeName = bridgeName;
  return currentBridgeName;
};

export const BridgeBadge = function BridgeBadge() {
  return bridgeName;
};
