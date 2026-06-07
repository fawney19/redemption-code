/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_API_BASE?: string
  readonly VITE_DEV_API_TARGET?: string
}

interface ImportMeta {
  readonly env: ImportMetaEnv
}

declare module 'lucide-vue-next' {
  import type { DefineComponent } from 'vue'

  type IconComponent = DefineComponent<Record<string, unknown>, Record<string, unknown>, unknown>

  export const Activity: IconComponent
  export const ChevronDown: IconComponent
  export const ChevronRight: IconComponent
  export const CircleAlert: IconComponent
  export const CircleCheck: IconComponent
  export const Copy: IconComponent
  export const Database: IconComponent
  export const Download: IconComponent
  export const FileCode: IconComponent
  export const FileJson: IconComponent
  export const Globe: IconComponent
  export const Info: IconComponent
  export const ListFilter: IconComponent
  export const Lock: IconComponent
  export const LogIn: IconComponent
  export const LogOut: IconComponent
  export const Play: IconComponent
  export const Plus: IconComponent
  export const RefreshCw: IconComponent
  export const RotateCcw: IconComponent
  export const Save: IconComponent
  export const Search: IconComponent
  export const Settings: IconComponent
  export const ShieldCheck: IconComponent
  export const Ticket: IconComponent
  export const Trash2: IconComponent
  export const Upload: IconComponent
}
