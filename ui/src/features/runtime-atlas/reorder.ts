export function movePath(order: string[], path: string, targetPath: string) {
  const source = order.indexOf(path);
  const target = order.indexOf(targetPath);
  if (source < 0 || target < 0 || source === target) return false;
  order.splice(target, 0, order.splice(source, 1)[0]);
  return true;
}

const nodeProcess = (globalThis as { process?: { argv: string[] } }).process;
if (nodeProcess?.argv[1]?.endsWith("/reorder.ts")) {
  const order = ["a", "b", "c"];
  if (!movePath(order, "c", "a") || !movePath(order, "c", "b") || order.join() !== "a,b,c") {
    throw new Error("worktree reorder self-check failed");
  }
}
