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
// UBRN payload enums are class instances. Retain the generated tag/inner data
// interfaces without importing their backend methods or runtime symbols.
for (const statement of source.statements) {
  if (!ts.isVariableStatement(statement)) continue;
  for (const variable of statement.declarationList.declarations) {
    if (!ts.isIdentifier(variable.name) || !variable.initializer) continue;
    const name = variable.name.text;
    const declaration = declarations.get(name);
    if (!declaration || !ts.isTypeAliasDeclaration(declaration)) continue;
    if (
      !ts.isTypeReferenceNode(declaration.type) ||
      !ts.isIdentifier(declaration.type.typeName) ||
      declaration.type.typeName.text !== "InstanceType"
    )
      continue;
    const variants = [];
    function visit(node) {
      if (
        ts.isTypeAliasDeclaration(node) &&
        node.name.text.endsWith("__interface") &&
        ts.isTypeLiteralNode(node.type) &&
        node.type.members.some(
          (member) =>
            ts.isPropertySignature(member) &&
            member.name.getText(source) === "tag",
        )
      )
        variants.push(node.type);
      ts.forEachChild(node, visit);
    }
    visit(variable.initializer);
    if (variants.length === 0)
      throw new Error(`Core payload enum has no data variants: ${name}`);
    declarations.set(
      name,
      ts.factory.updateTypeAliasDeclaration(
        declaration,
        declaration.modifiers,
        declaration.name,
        declaration.typeParameters,
        ts.factory.createUnionTypeNode(variants),
      ),
    );
  }
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
      if (ts.isIdentifier(node.typeName)) select(node.typeName.text);
      else if (
        ts.isQualifiedName(node.typeName) &&
        ts.isIdentifier(node.typeName.left) &&
        ts.isEnumDeclaration(declarations.get(node.typeName.left.text) ?? {})
      )
        select(node.typeName.left.text);
      else throw new Error("Core schema must not depend on backend namespaces");
    }
    if (ts.isImportTypeNode(node))
      throw new Error("Core schema must not import a backend");
    ts.forEachChild(node, visit);
  }
  ts.forEachChild(declaration, visit);
}
roots.forEach(select);
const printer = ts.createPrinter({ newLine: ts.NewLineKind.LineFeed });
const transform = ts.transform(
  [...selected.values()],
  [
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
  ],
);
const body = transform.transformed
  .map((node) => printer.printNode(ts.EmitHint.Unspecified, node, source))
  .join("\n\n");
fs.mkdirSync(path.dirname(output), { recursive: true });
fs.writeFileSync(
  output,
  `// Generated from Core UniFFI/UBRN. Do not edit.\n${body}\n`,
);
transform.dispose();
