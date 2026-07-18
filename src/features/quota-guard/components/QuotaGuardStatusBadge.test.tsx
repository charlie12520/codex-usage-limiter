// @vitest-environment jsdom
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { QuotaGuardStatusBadge } from "./QuotaGuardStatusBadge";

describe("QuotaGuardStatusBadge", () => {
  it("uses the supplied app-level opener without treating an unavailable projection as disabled", () => {
    const onOpen = vi.fn();
    render(<QuotaGuardStatusBadge state={null} onOpen={onOpen} />);

    fireEvent.click(screen.getByRole("button", { name: "Quota guard: Unavailable" }));
    expect(onOpen).toHaveBeenCalledOnce();
  });
});
