import { describe, expect, it } from "bun:test";
import type { EditTarget } from "../api";
import { addRow, baseId, buildChanges, cellValue, pendingCount, revertCell, setCell, toggleDelete, type EditMap } from "./edits";

const target: EditTarget = {
	schema: "public",
	table: "customers",
	columns: [{ name: "id", typeName: "int8" }, { name: "name", typeName: "text" }, null],
	key: [0],
};
const rows = [
	["1", "Ana", "x"],
	["2", "Bia", "y"],
];

describe("pending edits", () => {
	it("tracks updates and drops them when the value goes back", () => {
		const map: EditMap = {};
		setCell(map, baseId(0), 0, rows[0], 1, "Ana Maria");
		expect(cellValue(map, baseId(0), rows[0], 1)).toBe("Ana Maria");
		expect(pendingCount(map)).toBe(1);
		setCell(map, baseId(0), 0, rows[0], 1, "Ana");
		expect(pendingCount(map)).toBe(0);
	});

	it("builds update, delete and insert changes with original keys", () => {
		const map: EditMap = {};
		setCell(map, baseId(0), 0, rows[0], 1, null);
		toggleDelete(map, baseId(1), 1);
		const id = addRow(map, { 1: "Caio" });
		setCell(map, id, null, null, 0, "3");
		expect(buildChanges(target, rows, map)).toEqual([
			{ op: "update", key: ["1"], values: [{ column: 1, value: null }] },
			{ op: "delete", key: ["2"] },
			{ op: "insert", values: [{ column: 0, value: "3" }, { column: 1, value: "Caio" }] },
		]);
	});

	it("deleting a row replaces its edits and undeleting clears it", () => {
		const map: EditMap = {};
		setCell(map, baseId(1), 1, rows[1], 1, "Beatriz");
		toggleDelete(map, baseId(1), 1);
		expect(map[baseId(1)].op).toBe("delete");
		toggleDelete(map, baseId(1), 1);
		expect(pendingCount(map)).toBe(0);
	});

	it("removing a new row or reverting its last cell", () => {
		const map: EditMap = {};
		const id = addRow(map);
		toggleDelete(map, id, null);
		expect(pendingCount(map)).toBe(0);
		setCell(map, baseId(0), 0, rows[0], 1, "Z");
		revertCell(map, baseId(0), 1);
		expect(pendingCount(map)).toBe(0);
	});
});
