//! What each `doctor` lint means: the one-line summary `doctor --explain` lists, and the
//! full text it prints for one code (rustc's `--explain`, for Magento config).

use super::DoctorLint;

impl DoctorLint {
    /// One line: what the lint finds.
    pub fn summary(self) -> &'static str {
        match self {
            Self::ModuleMissingOnDisk => "a module enabled in config.php has no module.xml on disk",
            Self::ModuleNotRegistered => "a module on disk is missing from config.php",
            Self::SequenceCycle => "modules' <sequence> declarations form a cycle",
            Self::ModuleDisabled => "an installed module is disabled in config.php",
            Self::DisabledModuleReferenced => "enabled di.xml references a class of a disabled module",
            Self::PreferenceTargetMissing => "a <preference> points at a class that doesn't exist",
            Self::PreferenceCycle => "<preference> declarations form a cycle",
            Self::VirtualTypeBaseMissing => "a <virtualType> is based on a class that doesn't exist",
            Self::VirtualTypeCycle => "<virtualType> declarations form a cycle",
            Self::PluginClassMissing => "a <plugin> uses a class that doesn't exist",
            Self::DiArgumentClassMissing => "a di.xml argument injects a class that doesn't exist",
            Self::DuplicateArgumentKey => "an argument or array item is declared twice in one di.xml",
            Self::ObserverClassMissing => "an observer uses a class that doesn't exist",
            Self::CronInstanceMissing => "a cron job uses a class that doesn't exist",
            Self::WebapiServiceMissing => "a webapi route uses a service class that doesn't exist",
            Self::AclResourceUnknown => "a webapi route requires an ACL resource no acl.xml declares",
            Self::CommandClassMissing => "a console command uses a class that doesn't exist",
            Self::MqHandlerMissing => "a message queue handler or consumer class doesn't exist",
            Self::QueueNoConsumer => "a message queue has no consumer reading it",
            Self::GraphqlResolverMissing => "a GraphQL resolver class doesn't exist",
            Self::TemplateFileMissing => "layout assigns a template no module or theme provides",
            Self::CommandUnregistered => "a console command class is registered nowhere",
            Self::ObserverUnregistered => "an observer class is registered in no events.xml",
            Self::PluginUnregistered => "a plugin class is declared in no di.xml",
        }
    }

    /// The full explanation: what triggers the lint, why it matters, and how to fix it.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::ModuleMissingOnDisk => "\
app/etc/config.php enables a module, but no etc/module.xml for it exists in app/code or
in any installed package. Usually the package was removed with composer (or the
directory deleted) without updating config.php.

config.php and the code on disk now disagree about which modules exist, which setup
and deployment commands trip over.

Fix: remove the module's line from app/etc/config.php, or reinstall the package if it
was removed by accident.",

            Self::ModuleNotRegistered => "\
A module's etc/module.xml and registration.php exist on disk, but app/etc/config.php
doesn't list it. Magento only loads modules named in config.php, so none of this
module's configuration is active.

Fix: run `bin/magento setup:upgrade` (it adds new modules to config.php) and commit the
changed config.php. If the directory is left over and shouldn't be a module, delete it,
or move any plain PHP classes into a module that is registered.",

            Self::SequenceCycle => "\
Each module's <sequence> in etc/module.xml names modules that must load before it. Here
those declarations loop (A after B, B after A), so no valid load order exists, and
Magento fails to sort the modules.

Fix: remove the <sequence> entry that isn't actually needed. A module only needs to
sequence after the modules whose configuration it overrides.",

            Self::ModuleDisabled => "\
A module is installed but set to 0 in app/etc/config.php. Its code is still on disk and
autoloadable, so whether it takes part in a class hierarchy, a plugin list or a DI
compile depends on the tool: setup:di:compile, the runtime and other tools disagree at
exactly these edges.

Fix: remove the module with composer instead (`composer remove vendor/package`). For
Magento's own modules, add them to the `replace` section of the root composer.json.",

            Self::DisabledModuleReferenced => "\
Enabled di.xml (a plugin target or class, a preference, a virtualType base or an
injected argument) references a class whose file lives in a module disabled in
config.php. The disabled module's configuration is gone but its classes still load,
so this config half-uses a module that is switched off.

Fix: remove the disabled module with composer (see E1004), or remove the configuration
that reaches into it.",

            Self::PreferenceTargetMissing => "\
A <preference for=\"A\" type=\"B\"/> makes the object manager build B whenever A is
asked for, but B doesn't exist. Anything that asks for A fails with \"Class B does not
exist\".

Severity follows reachability. It is an error when A exists, because then code can
type-hint it. If A doesn't exist either, nothing can ask for it: a warning when B's
package is installed (stale config), an info when no installed package provides B (an
optional integration with a package you don't use). The chain under the finding shows
which.

Fix: point the preference at the class that replaced B, or remove it.",

            Self::PreferenceCycle => "\
Following <preference> declarations from one type leads back to it (A → B → A), so the
object manager can never settle on a class to build.

Fix: remove one of the preferences in the loop.",

            Self::VirtualTypeBaseMissing => "\
A <virtualType name=\"V\" type=\"B\"/> builds V as a configured copy of B, but B doesn't
exist. Anything that asks for V fails.

A virtual type is only built when something asks for it by name, so doctor follows every
reference to V back to something that can be built: an error when one exists, otherwise
a warning (B's package is installed: stale config) or an info (no installed package
provides B: an optional integration). The chain under the finding shows each path.
References from PHP code or from layout and UI component XML aren't followed.

Fix: base the virtual type on the class that replaced B, or remove it and its
references.",

            Self::VirtualTypeCycle => "\
A chain of <virtualType> declarations leads back to itself (V1 based on V2, V2 on V1),
so there is no real class to build.

Fix: base one of the virtual types on a real class.",

            Self::PluginClassMissing => "\
A <plugin type=\"P\"/> declares P as an interceptor, but P doesn't exist. Every call to
the intercepted class fails once the interceptor is generated.

It is an error when the intercepted type exists; when it doesn't, nothing can call it:
a warning (P's package is installed) or an info (no installed package provides P).

Fix: point the plugin at the right class, remove it, or disable it with
<plugin name=\"...\" disabled=\"true\"/> from your own module.",

            Self::DiArgumentClassMissing => "\
A di.xml <argument xsi:type=\"object\"> (or an array item of that type) injects a class
that doesn't exist. Building the configured type then fails.

The argument only reaches the constructor when the configured type can be built AND its
constructor declares that parameter, so doctor checks both: an error when it does, else
a warning (the class's package is installed: stale config) or an info (no installed
package provides it: an optional integration). The chain shows the dead end, such as
\"X's constructor has no $foo parameter\" or \"X does not exist\".

Fix: inject the class that replaced it, or remove the argument.",

            Self::DuplicateArgumentKey => "\
One di.xml declares the same <argument name> or array <item name> twice for one type.
Magento merges a file the way its DOM merge works: two arrays with the same key combine
item by item, but any other repeat is replaced by the later declaration. So the earlier
value is silently lost; for example, a console command whose item name was copy-pasted
never appears in bin/magento:

    <item name=\"reindex\" xsi:type=\"object\">Acme\\Shop\\Console\\Reindex</item>
    <item name=\"reindex\" xsi:type=\"object\">Acme\\Shop\\Console\\Cleanup</item>   <- only this one

Repeats with identical values are not reported. Across files, repeating a key is normal
Magento override behaviour and is not reported either.

Fix: give each item a unique name, or delete the declaration that shouldn't be there.",

            Self::ObserverClassMissing => "\
An events.xml <observer instance=\"O\"/> uses a class that doesn't exist. When the event
is dispatched, building the observer fails and the whole request errors.

This stays an error even when nothing seems to dispatch the event: event names are often
built at runtime, so that can't be known statically.

Fix: point the observer at the right class, remove it, or disable it with
<observer name=\"...\" disabled=\"true\"/> from your own module.",

            Self::CronInstanceMissing => "\
A crontab.xml job's instance class doesn't exist, so every run of the job fails (and is
logged as an error in cron_schedule).

Fix: point the job at the right class, or remove it.",

            Self::WebapiServiceMissing => "\
A webapi.xml route's <service class> doesn't exist, so every call to that REST/SOAP
endpoint fails.

Fix: point the route at the right service interface, or remove it.",

            Self::AclResourceUnknown => "\
A webapi.xml route requires an ACL resource that no acl.xml declares. Only users and
integrations with full (\"All\") access can call it; an integration granted specific
resources can never be given access, because the resource isn't in the tree.

Fix: declare the resource in the module's etc/acl.xml and reference that id, or use an
existing resource. `anonymous` and `self` are built in and never reported.",

            Self::CommandClassMissing => "\
A console command registered on CommandListInterface in di.xml uses a class that
doesn't exist, so bin/magento itself fails to start.

Fix: point the item at the right class, or remove it.",

            Self::MqHandlerMissing => "\
A message queue topic handler, consumer handler or consumerInstance names a class that
doesn't exist, so consuming the queue fails.

Fix: point it at the right class, or remove the handler or consumer.",

            Self::QueueNoConsumer => "\
A queue is bound to a topic but no consumer reads it, so published messages pile up
forever.

Fix: add a consumer in queue_consumer.xml, or remove the binding if nothing should be
published there.",

            Self::GraphqlResolverMissing => "\
A schema.graphqls @resolver or @typeResolver names a class that doesn't exist, so
queries touching that field or type fail.

Fix: point the directive at the right class.",

            Self::TemplateFileMissing => "\
Layout XML assigns a template (template=\"Vendor_Module::path.phtml\", or a setTemplate
action) that no module's view/<area>/templates directory and no theme override
provides. The block renders nothing and Magento logs \"Invalid template file\"; in
developer mode it throws.

Fix: add the template file, correct the path, or remove the block or the setTemplate
action.",

            Self::CommandUnregistered => "\
A class under Console/ extends Symfony's Command but is registered on no
CommandListInterface in di.xml, so it never appears in bin/magento.

This is often dead code, so it's a warning; check for a typo'd or duplicated item name
in di.xml first (see E2007).

Fix: register it as an item of Magento\\Framework\\Console\\CommandListInterface's
$commands argument, or delete the class.",

            Self::ObserverUnregistered => "\
A class under Observer/ implements ObserverInterface but no events.xml registers it, so
it never runs.

Fix: register it on the right event in etc/events.xml (or an area's events.xml), or
delete the class.",

            Self::PluginUnregistered => "\
A class under Plugin/ defines before/around/after methods but no di.xml declares it as a
<plugin>, so it never intercepts anything. A plugin whose declaration was commented out
also shows up here.

Fix: declare it in di.xml on the type it should intercept, or delete the class.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DoctorLint;
    use std::collections::HashSet;

    // Codes are a public contract (CI configs `--deny` them): each is unique, well-formed,
    // parses back to its lint, and comes with an explanation.
    #[test]
    fn every_code_is_unique_parseable_and_explained() {
        let mut seen = HashSet::new();
        for lint in DoctorLint::ALL {
            let code = lint.code();
            assert!(seen.insert(code), "{code} used twice");
            assert!(code.len() == 5 && code.starts_with('E') && code[1..].bytes().all(|b| b.is_ascii_digit()));
            assert_eq!(code.parse::<DoctorLint>(), Ok(lint));
            assert_eq!(code.to_lowercase().parse::<DoctorLint>(), Ok(lint));
            assert_eq!(lint.to_string().parse::<DoctorLint>(), Ok(lint));
            assert!(!lint.summary().is_empty() && lint.explanation().contains("Fix:"), "{code}");
        }
    }
}
