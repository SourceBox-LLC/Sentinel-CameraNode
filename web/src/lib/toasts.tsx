// Tiny toast system — no dependency.  A single context provides
// `showToast(msg, kind)`; the <ToastStack/> component subscribes and
// renders the visible stack with auto-dismiss.

import {
  createContext,
  ReactNode,
  useCallback,
  useContext,
  useEffect,
  useState,
} from "react"

type ToastKind = "success" | "error" | "info"

interface Toast {
  id: number
  message: string
  kind: ToastKind
}

interface ToastContext {
  showToast: (message: string, kind?: ToastKind) => void
}

const Ctx = createContext<ToastContext>({ showToast: () => undefined })

let nextId = 1

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([])

  const showToast = useCallback((message: string, kind: ToastKind = "info") => {
    const id = nextId++
    setToasts((prev) => [...prev, { id, message, kind }])
  }, [])

  return (
    <Ctx.Provider value={{ showToast }}>
      {children}
      <ToastStack toasts={toasts} onDismiss={(id) => setToasts((p) => p.filter((t) => t.id !== id))} />
    </Ctx.Provider>
  )
}

export function useToasts() {
  return useContext(Ctx)
}

function ToastStack({ toasts, onDismiss }: { toasts: Toast[]; onDismiss: (id: number) => void }) {
  return (
    <div className="toast-stack" aria-live="polite">
      {toasts.map((t) => (
        <ToastItem key={t.id} toast={t} onDismiss={() => onDismiss(t.id)} />
      ))}
    </div>
  )
}

function ToastItem({ toast, onDismiss }: { toast: Toast; onDismiss: () => void }) {
  useEffect(() => {
    const id = setTimeout(onDismiss, 3500)
    return () => clearTimeout(id)
  }, [onDismiss])
  return <div className={`toast ${toast.kind}`}>{toast.message}</div>
}
