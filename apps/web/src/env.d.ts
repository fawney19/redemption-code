/// <reference types="vite/client" />

declare module 'lucide-vue-next' {
  import type { DefineComponent } from 'vue'

  type IconComponent = DefineComponent<Record<string, unknown>, Record<string, unknown>, unknown>

  export const Activity: IconComponent
  export const CircleAlert: IconComponent
  export const CircleCheck: IconComponent
  export const Database: IconComponent
  export const Download: IconComponent
  export const FileCode: IconComponent
  export const FileJson: IconComponent
  export const Info: IconComponent
  export const Lock: IconComponent
  export const LogIn: IconComponent
  export const LogOut: IconComponent
  export const Plus: IconComponent
  export const RefreshCw: IconComponent
  export const RotateCcw: IconComponent
  export const Search: IconComponent
  export const ShieldCheck: IconComponent
  export const Ticket: IconComponent
  export const Upload: IconComponent
}
