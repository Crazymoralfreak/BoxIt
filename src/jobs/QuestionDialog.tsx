// What a running operation stopped to ask: a password, what to do with a
// file that is already there, whether to create a symbolic link.

import { useState, type FormEvent } from "react";
import { Dialog } from "../components/Dialog";
import { baseName, useFormat } from "../format";
import { useT } from "../i18n";
import type { ConflictChoice, Question, Reply } from "../types";

type PasswordQuestion = Extract<Question, { kind: "password" }>;
type ConflictQuestion = Extract<Question, { kind: "conflict" }>;
type SymlinkQuestion = Extract<Question, { kind: "symlink" }>;

function PasswordQuestionDialog({
  question,
  onReply,
}: {
  question: PasswordQuestion;
  onReply: (reply: Reply) => void;
}) {
  const { t } = useT();
  const [password, setPassword] = useState("");
  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (password !== "") {
      onReply({ kind: "password", password });
    }
  };
  const skip = () => onReply({ kind: "password", password: null });
  return (
    <Dialog
      title={t("question.passwordTitle")}
      onClose={skip}
      footer={
        <>
          <button type="button" onClick={skip}>
            {t("question.passwordSkip")}
          </button>
          <button type="submit" form="question-password" className="primary" disabled={password === ""}>
            {t("common.ok")}
          </button>
        </>
      }
    >
      <form id="question-password" className="form" onSubmit={submit}>
        {question.retry && <p className="notice error">{t("question.passwordWrong")}</p>}
        <label className="field">
          <span>{t("question.passwordFor", { name: question.entry ?? baseName(question.archive) })}</span>
          <input
            type="password"
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            autoComplete="off"
            data-autofocus
          />
        </label>
      </form>
    </Dialog>
  );
}

function ConflictQuestionDialog({
  question,
  onReply,
}: {
  question: ConflictQuestion;
  onReply: (reply: Reply) => void;
}) {
  const { t } = useT();
  const format = useFormat();
  const [all, setAll] = useState(false);
  const answer = (choice: ConflictChoice) => onReply({ kind: "conflict", choice, all });
  const described = (size: number | null, modified: number | null) =>
    [size === null ? null : format.size(size), modified === null ? null : format.date(modified)]
      .filter((part) => part !== null)
      .join(", ");
  return (
    <Dialog
      title={t("question.conflictTitle")}
      wide
      footer={
        <>
          <button type="button" className="quiet" onClick={() => answer("cancel")}>
            {t("question.conflictCancel")}
          </button>
          <span className="spacer" />
          <button type="button" onClick={() => answer("rename")}>
            {t("question.conflictRename")}
          </button>
          <button type="button" onClick={() => answer("overwrite")}>
            {t("question.conflictOverwrite")}
          </button>
          <button type="button" className="primary" onClick={() => answer("skip")} data-autofocus>
            {t("question.conflictSkip")}
          </button>
        </>
      }
    >
      <div className="form">
        <p className="path-text">{question.path}</p>
        <dl className="facts">
          <dt>{t("question.conflictExisting")}</dt>
          <dd>{described(question.existingSize, question.existingModified)}</dd>
          <dt>{t("question.conflictIncoming")}</dt>
          <dd>{described(question.size, question.modified)}</dd>
        </dl>
        <label className="check">
          <input type="checkbox" checked={all} onChange={(event) => setAll(event.target.checked)} />
          <span>{t("question.conflictAll")}</span>
        </label>
      </div>
    </Dialog>
  );
}

function SymlinkQuestionDialog({
  question,
  onReply,
}: {
  question: SymlinkQuestion;
  onReply: (reply: Reply) => void;
}) {
  const { t } = useT();
  return (
    <Dialog
      title={t("question.symlinkTitle")}
      footer={
        <>
          <button type="button" onClick={() => onReply({ kind: "symlink", create: true })}>
            {t("question.symlinkCreate")}
          </button>
          <button
            type="button"
            className="primary"
            onClick={() => onReply({ kind: "symlink", create: false })}
            data-autofocus
          >
            {t("question.symlinkSkip")}
          </button>
        </>
      }
    >
      <div className="form">
        <p>{t("question.symlinkText")}</p>
        <p className="path-text">
          {question.name} → {question.target}
        </p>
      </div>
    </Dialog>
  );
}

export default function QuestionDialog({
  question,
  onReply,
}: {
  question: Question;
  onReply: (reply: Reply) => void;
}) {
  switch (question.kind) {
    case "password":
      return <PasswordQuestionDialog question={question} onReply={onReply} />;
    case "conflict":
      return <ConflictQuestionDialog question={question} onReply={onReply} />;
    case "symlink":
      return <SymlinkQuestionDialog question={question} onReply={onReply} />;
    default: {
      const unknown: never = question;
      throw new Error(`unknown question: ${String(unknown)}`);
    }
  }
}
