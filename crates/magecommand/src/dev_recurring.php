<?php
// magecommand dev upgrade: run only the recurring setup scripts of the modules
// given (a JSON list in argv[1], in module-list order), the way
// Installer::installSchema and Installer::installDataFixtures run them, after
// magecommand has done every other setup:upgrade step natively.
//
// Piped to PHP on stdin (`php -- '<json>'`) from the store root; nothing is
// written into the store. It calls the Installer's own helpers for the class
// lookup, the interface check and the module contexts, so a script runs exactly
// as setup:upgrade would run it.
declare(strict_types=1);

$modules = json_decode($argv[1] ?? '', true, 8, JSON_THROW_ON_ERROR);

require getcwd() . '/app/bootstrap.php';

$output = new \Symfony\Component\Console\Output\ConsoleOutput();
try {
    $cli = new \Magento\Framework\Console\Cli('Magento CLI');
    $command = $cli->find('setup:upgrade');
    $get = static fn (object $o, string $property) => (fn () => $this->$property)->call($o);
    $installer = $get($command, 'installerFactory')
        ->create(new \Magento\Framework\Setup\ConsoleLogger($output));
    $call = static fn (string $method, ...$args) => (fn () => $this->$method(...$args))->call($installer);

    $registry = $get($installer, 'objectManagerProvider')->get()->get(\Magento\Framework\Registry::class);
    $resource = $installer->getModuleResource();
    foreach (['schema', 'data'] as $type) {
        // For backward compatibility in install and upgrade scripts with enabled parallelization.
        $registry->register('setup-mode-enabled', true);
        $setup = $type === 'schema'
            ? $get($installer, 'setupFactory')->create($get($installer, 'context')->getResources())
            : $get($installer, 'dataSetupFactory')->create();
        $contexts = $call('generateListOfModuleContext', $resource, "$type-version");
        foreach ($modules as $module) {
            $handler = $call('getSchemaDataHandler', $module, "$type-recurring");
            if ($handler) {
                $output->writeln("Module '$module': running $type recurring");
                $handler->install($setup, $contexts[$module] ?? new \Magento\Setup\Model\ModuleContext(''));
            }
        }
        $registry->unregister('setup-mode-enabled');
    }
} catch (\Throwable $e) {
    $output->writeln('<error>' . $e->getMessage() . '</error>');
    exit(1);
}
