import { open } from "@tauri-apps/plugin-dialog";

export const pickDirectory = () => open({ directory: true, multiple: false });
