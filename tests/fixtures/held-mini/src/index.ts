export function openDatabase(): void {
  const handle = NativeHeldCore.openDatabase();
  void handle;
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
