"""Bridge Meet's independent lockfile to React's local Web SDK dependency.

The lock translator emits an empty dependency map for its linked React package.
Use the maintained linker and the existing SDK output, without a version alias.
"""

load("@aspect_rules_js//npm:defs.bzl", "npm_link_package")

def link_web_for_meet(name, prod = True, dev = True):
    if not prod:
        return [], {}
    link = npm_link_package(
        name = name + "/@pulsebeam/web",
        package = "@pulsebeam/web",
        src = Label("//agents/pulsebeam-agent-web:package"),
    )
    return [link], {"@pulsebeam": [link]}
