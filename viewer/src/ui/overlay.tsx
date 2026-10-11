// Modal dialog (native <dialog>) and toasts.
import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import * as Icon from "./icons";

export function Dialog({ open, onClose, title, children, footer, width }: { open: boolean; onClose: () => void; title: ReactNode; children: ReactNode; footer?: ReactNode; width?: number }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    if (!open && d.open) d.close();
  }, [open]);
  return (
    <dialog
      ref={ref}
      className="dialog"
      style={width ? { width: `min(${width}px, calc(100vw - 2rem))` } : undefined}
      onClose={onClose}
      onClick={(e) => {
        if (e.target === ref.current) onClose(); // backdrop
      }}
    >
      {open && (
        <>
          <div className="dialog__head">
            <h2 style={{ fontSize: "var(--text-lg)" }}>{title}</h2>
            <button className="btn btn--ghost btn--icon btn--sm" onClick={onClose} aria-label="Close">
              <Icon.Close size={14} />
            </button>
          </div>
          <div className="dialog__body">{children}</div>
          {footer && <div className="dialog__foot">{footer}</div>}
        </>
      )}
    </dialog>
  );
}

interface Toast {
  id: number;
  text: ReactNode;
  kind?: "bad";
  action?: { label: string; run: () => void };
}

const ToastCtx = createContext<(t: Omit<Toast, "id">, ms?: number) => void>(() => {});

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const push = useCallback((t: Omit<Toast, "id">, ms = 5000) => {
    const id = Date.now() + Math.random();
    setToasts((x) => [...x, { ...t, id }]);
    setTimeout(() => setToasts((x) => x.filter((y) => y.id !== id)), ms);
  }, []);
  return (
    <ToastCtx.Provider value={push}>
      {children}
      <div className="toasts" role="status" aria-live="polite">
        {toasts.map((t) => (
          <div key={t.id} className={`toast${t.kind ? ` toast--${t.kind}` : ""}`}>
            <span style={{ flex: 1 }}>{t.text}</span>
            {t.action && (
              <button
                className="btn btn--sm"
                onClick={() => {
                  t.action!.run();
                  setToasts((x) => x.filter((y) => y.id !== t.id));
                }}
              >
                {t.action.label}
              </button>
            )}
          </div>
        ))}
      </div>
    </ToastCtx.Provider>
  );
}

export const useToast = () => useContext(ToastCtx);
