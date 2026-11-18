import fs from "node:fs";
import path from "node:path";
import ts from "typescript";

const [input, output, ...roots] = process.argv.slice(2);
if (!input || !output || roots.length === 0)
  throw new Error("Expected Core bindings, schema output, and exported types");
const source = ts.createSourceFile(
  input,
  fs.readFileSync(input, "utf8"),
  ts.ScriptTarget.Latest,
  true,
  ts.ScriptKind.TS,
);
const declarations = new Map();
for (const statement of source.statements) {
  if (
    ts.isTypeAliasDeclaration(statement) ||
    ts.isInterfaceDeclaration(statement) ||
    ts.isEnumDeclaration(statement)
  )
    declarations.set(statement.name.text, statement);
}
const selected = new Map();
const builtins = new Set(["Array", "ReadonlyArray", "Readonly", "Record"]);
function select(name) {
  if (selected.has(name) || builtins.has(name)) return;
  const declaration = declarations.get(name);
  if (!declaration) throw new Error(`Core schema type not found: ${name}`);
  selected.set(name, declaration);
  function visit(node) {
    if (ts.isTypeReferenceNode(node)) {
      if (!ts.isIdentifier(node.typeName))
        throw new Error("Core schema must not depend on backend namespaces");
      select(node.typeName.text);
    }
    if (ts.isImportTypeNode(node))
      throw new Error("Core schema must not import a backend");
    ts.forEachChild(node, visit);
  }
  ts.forEachChild(declaration, visit);
}
roots.forEach(select);
const printer = ts.createPrinter({ newLine: ts.NewLineKind.LineFeed });
const transform = ts.transform([...selected.values()], [
  (context) => {
    function visit(node) {
      if (ts.isPropertySignature(node)) {
        return ts.factory.updatePropertySignature(
          node,
          [ts.factory.createModifier(ts.SyntaxKind.ReadonlyKeyword)],
          node.name,
          node.questionToken,
          ts.visitNode(node.type, visit),
        );
      }
      return ts.visitEachChild(node, visit, context);
    }
    return (node) => ts.visitNode(node, visit);
  },
]);
const body = transform.transformed
  .map((node) => printer.printNode(ts.EmitHint.Unspecified, node, source))
  .join("\n\n");
fs.mkdirSync(path.dirname(output), { recursive: true });
fs.writeFileSync(output, `// Generated from Core UniFFI/UBRN. Do not edit.\n${body}\n`);
transform.dispose();
