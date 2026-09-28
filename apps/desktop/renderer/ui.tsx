import { useEffect, useRef, type ReactNode, type ComponentType, type ButtonHTMLAttributes } from 'react';
import { X, LoaderCircle } from 'lucide-react';
import type { LucideProps } from 'lucide-react';

export function IconButton({ icon: Icon, label, ...props }: ButtonHTMLAttributes<HTMLButtonElement> & { icon: ComponentType<LucideProps>; label: string }) {
  return <button type="button" {...props} className={`icon-button ${props.className || ''}`} aria-label={label} title={label}><Icon size={18} strokeWidth={1.6} /></button>;
}
export function Modal({ title, children, onClose, className = '' }: { title: string; children: ReactNode; onClose: () => void; className?: string }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => { ref.current?.showModal(); }, []);
  return <dialog ref={ref} className={`modal ${className}`} aria-label={title} onCancel={event => { event.preventDefault(); onClose(); }} onClick={event => { if (event.target === event.currentTarget) onClose(); }}>
    <div className="modal-inner"><header><h2>{title}</h2><IconButton icon={X} label="关闭" onClick={onClose} /></header>{children}</div>
  </dialog>;
}
export function Loading({ label = '正在加载…' }: { label?: string }) { return <div className="loading" role="status"><LoaderCircle size={17} className="spin" />{label}</div>; }
export function Empty({ icon: Icon, title, children }: { icon: ComponentType<LucideProps>; title: string; children?: ReactNode }) {
  return <div className="empty"><Icon size={30} strokeWidth={1.3} /><h2>{title}</h2>{children && <p>{children}</p>}</div>;
}
