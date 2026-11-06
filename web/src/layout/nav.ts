import type { IconName } from "../components/icons";
import { msg } from "../i18n";

export type NavItem = readonly [to: string, label: string, icon: IconName];

export interface NavGroup {
  label: string;
  items: readonly NavItem[];
}

/** The sidebar, in order; the command palette lists the same pages. */
export const NAV_GROUPS: readonly NavGroup[] = [
  {
    label: msg("Deploy"),
    items: [
      ["/", msg("Overview"), "overview"],
      ["/projects", msg("Projects"), "folder"],
      ["/templates", msg("Templates"), "layers"],
      ["/servers", msg("Servers"), "server"],
    ],
  },
  {
    label: msg("Observe"),
    items: [
      ["/monitoring", msg("Monitoring"), "activity"],
      ["/load", msg("Load"), "gauge"],
      ["/logs", msg("Logs"), "logs"],
      ["/alerts", msg("Alerts"), "bell"],
    ],
  },
  {
    label: msg("S3"),
    items: [
      ["/s3-destinations", msg("S3 destinations"), "bucket"],
      ["/rudder-image", msg("Rudder Image"), "box"],
      ["/backups", msg("Backups"), "archive"],
    ],
  },
  {
    label: msg("Network & security"),
    items: [
      ["/security", msg("Security"), "shield"],
      ["/certificates", msg("Certificates"), "lock"],
      ["/cache", msg("Cache"), "zap"],
    ],
  },
  {
    label: msg("Integrations"),
    items: [
      ["/git", msg("Git providers"), "git"],
      ["/registries", msg("Registries"), "box"],
    ],
  },
  {
    label: msg("Admin"),
    items: [
      ["/users", msg("Users"), "users"],
      ["/access-admin", msg("Rudder Access"), "lock"],
      ["/tokens", msg("API tokens"), "key"],
      ["/agents", msg("AI agents"), "command"],
      ["/audit", msg("Audit log"), "history"],
      ["/system", msg("System"), "settings"],
    ],
  },
];
