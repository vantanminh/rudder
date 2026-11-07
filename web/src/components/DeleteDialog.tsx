import { useState, type FormEvent, type ReactNode } from "react";
import { useT } from "../i18n";
import { Button, Dialog, Field, TextInput } from "./ui";

/** Asks the person to type the name of what they are about to delete. */
export function DeleteDialog({
  title,
  name,
  pending,
  onClose,
  onConfirm,
  children,
}: {
  title: ReactNode;
  /** The exact name that has to be typed to enable the delete button. */
  name: string;
  pending: boolean;
  onClose: () => void;
  onConfirm: () => void;
  children: ReactNode;
}) {
  const t = useT();
  const [typed, setTyped] = useState("");
  return (
    <Dialog open onClose={onClose} title={title} description={children}>
      <form
        className="grid gap-5"
        onSubmit={(event: FormEvent) => {
          event.preventDefault();
          if (typed === name) onConfirm();
        }}
      >
        <Field label={t("Type {name} to confirm", { name })}>
          <TextInput
            autoFocus
            autoComplete="off"
            value={typed}
            onChange={(event) => setTyped(event.target.value)}
          />
        </Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="secondary" onClick={onClose}>
            {t("Cancel")}
          </Button>
          <Button type="submit" variant="danger-solid" disabled={typed !== name || pending}>
            {t("Delete")}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
