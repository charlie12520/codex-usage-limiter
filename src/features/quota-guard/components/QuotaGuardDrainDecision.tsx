type Props = {
  onKeepWaiting: () => void;
  onInterruptNow: () => void;
  busy?: boolean;
};

export function QuotaGuardDrainDecision({ onKeepWaiting, onInterruptNow, busy = false }: Props) {
  return (
    <div className="modal-actions">
      <button type="button" className="ghost" disabled={busy} onClick={onKeepWaiting}>
        Keep waiting
      </button>
      <button type="button" className="danger" disabled={busy} onClick={onInterruptNow}>
        Interrupt now
      </button>
    </div>
  );
}
