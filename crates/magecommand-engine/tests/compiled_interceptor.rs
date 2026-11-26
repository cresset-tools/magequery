//! creatuity/magento2-interceptors' compiled interceptor format, pinned on a
//! small fixture: one class with a global before/around/after plugin and a
//! frontend-only plugin, so the output carries the per-scope `switch` (scopes
//! with equal chains share a `case`, the last group is `default:`), a nested
//! around closure, a `void` method with `break;`, the `_empty` default for
//! scopes without plugins, the `__sleep`/`__wakeup` overrides, and a parent
//! constructor parameter reused for `____om`.
//!
//! Matched byte for byte against PHP's output on Mage-OS 4.0.0 (516/516
//! interceptors of a large store) via `examples/compiled_interceptor_compare.rs`.

use std::path::Path;

use magecommand_engine::compiled_interceptor::{compiled_interceptor_bytes, AreasPluginList};
use magecommand_engine::definitions::Definitions;
use magequery_core::Magento;

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(&p, body).expect("write");
}

const DI_HEAD: &str = r#"<?xml version="1.0"?><config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">"#;

#[test]
fn compiled_interceptor_format() {
    let dir = std::env::temp_dir().join(format!("mc-compiled-interceptor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.as_path();

    put(root, "app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Base' => 1]];\n");
    put(
        root,
        "app/code/Acme/Base/etc/module.xml",
        r#"<?xml version="1.0"?><config><module name="Acme_Base"/></config>"#,
    );
    put(
        root,
        "app/code/Acme/Base/registration.php",
        "<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\\Magento\\Framework\\Component\\ComponentRegistrar::MODULE, 'Acme_Base', __DIR__);\n",
    );
    put(
        root,
        "app/code/Acme/Base/etc/di.xml",
        &format!(
            r#"{DI_HEAD}
<type name="Magento\Framework\App\AreaList"><arguments><argument name="areas" xsi:type="array">
  <item name="frontend" xsi:type="array"><item name="frontName" xsi:type="null"/></item>
  <item name="adminhtml" xsi:type="array"><item name="frontName" xsi:type="string">admin</item></item>
</argument></arguments></type>
<type name="Acme\Base\Model\Thing"><plugin name="acme-p" type="Acme\Base\Plugin\P"/></type>
</config>"#
        ),
    );
    put(
        root,
        "app/code/Acme/Base/etc/frontend/di.xml",
        &format!(
            r#"{DI_HEAD}<type name="Acme\Base\Model\Thing"><plugin name="acme_f" type="Acme\Base\Plugin\F" sortOrder="10"/></type></config>"#
        ),
    );
    put(
        root,
        "app/code/Acme/Base/Model/Thing.php",
        r#"<?php
namespace Acme\Base\Model;

class Thing
{
    public function __construct(\Magento\Framework\ObjectManagerInterface $objectManager, $name = 'x')
    {
    }

    public function run($x)
    {
        return $x;
    }

    public function stop(): void
    {
    }

    public function untouched()
    {
    }

    public function __sleep()
    {
        return [];
    }

    public static function make()
    {
    }
}
"#,
    );
    put(
        root,
        "app/code/Acme/Base/Plugin/P.php",
        r#"<?php
namespace Acme\Base\Plugin;

class P
{
    public function beforeRun($subject, $x)
    {
    }

    public function aroundRun($subject, callable $proceed, $x)
    {
        return $proceed($x);
    }

    public function afterRun($subject, $result)
    {
        return $result;
    }

    public function afterMake($subject, $result)
    {
        return $result;
    }
}
"#,
    );
    put(
        root,
        "app/code/Acme/Base/Plugin/F.php",
        r#"<?php
namespace Acme\Base\Plugin;

class F
{
    public function afterRun($subject, $result)
    {
        return $result;
    }

    public function beforeStop($subject)
    {
    }
}
"#,
    );

    let magento = Magento::open(root).expect("open");
    let subjects = vec!["Acme\\Base\\Model\\Thing".to_owned()];
    let mut defs = Definitions::empty();
    defs.load_closure(&magento, root, subjects.iter().cloned());
    let need = AreasPluginList::required_classes(&magento, &defs, &subjects);
    defs.load_closure(&magento, root, need);

    let plugins = AreasPluginList::new(&magento, &defs, &subjects);
    assert_eq!(plugins.scope_codes(), ["global", "frontend", "adminhtml", "primary"]);

    let got = compiled_interceptor_bytes(&defs, &plugins, "Acme\\Base\\Model\\Thing").expect("rendered");
    let want = r#"<?php
namespace Acme\Base\Model\Thing;

use Magento\Framework\Config\ScopeInterface;
use Magento\Framework\ObjectManagerInterface;

/**
 * Interceptor class for @see \Acme\Base\Model\Thing
 */
class Interceptor extends \Acme\Base\Model\Thing
{
    /**
     * @var ScopeInterface
     */
    private $____scope = null;

    /**
     * @var ObjectManagerInterface
     */
    private $____om = null;

    /**
     * @var \Acme\Base\Plugin\P
     */
    private $____plugin_acme_p = null;

    /**
     * @var \Acme\Base\Plugin\F
     */
    private $____plugin_acme_f = null;

    /**
     * {@inheritdoc}
     */
    public function __construct(\Magento\Framework\Config\ScopeInterface $____scope, \Magento\Framework\ObjectManagerInterface $objectManager, $name = 'x')
    {
        $this->____om = $objectManager;
        $this->____scope = $____scope;
        parent::__construct($objectManager, $name);
    }

    /**
     * {@inheritdoc}
     */
    public function __sleep()
    {
        $properties = parent::__sleep();
        return \array_diff(
        	$properties,
        	[
        		'____scope',
        		'____om',
        	]
        );
    }

    /**
     * {@inheritdoc}
     */
    public function run($x)
    {
        switch ($this->____scope->getCurrentScope()) {
        	case 'global':
        	case 'adminhtml':
        	case 'primary':
        		$arguments = \func_get_args();
        		$beforeResult = $this->____plugin_acme_p()->beforeRun($this, ...\array_values($arguments));
        		if ($beforeResult !== null) $arguments = (array)$beforeResult;
        		
        		$result = $this->____plugin_acme_p()->aroundRun($this, function(...$arguments){
        			$arguments = \func_get_args();
        			return parent::run(...\array_values($arguments));
        		}, ...\array_values($arguments));
        		
        		return $this->____plugin_acme_p()->afterRun($this, $result, ...\array_values($arguments));
        	default:
        		$arguments = \func_get_args();
        		$beforeResult = $this->____plugin_acme_p()->beforeRun($this, ...\array_values($arguments));
        		if ($beforeResult !== null) $arguments = (array)$beforeResult;
        		
        		$result = $this->____plugin_acme_p()->aroundRun($this, function(...$arguments){
        			$arguments = \func_get_args();
        			$result = parent::run(...\array_values($arguments));
        			
        			return $this->____plugin_acme_f()->afterRun($this, $result, ...\array_values($arguments));
        		}, ...\array_values($arguments));
        		
        		return $this->____plugin_acme_p()->afterRun($this, $result, ...\array_values($arguments));
        }
    }

    /**
     * {@inheritdoc}
     */
    public function stop(): void
    {
        switch ($this->____scope->getCurrentScope()) {
        	case 'frontend':
        		$arguments = \func_get_args();
        		$this->____plugin_acme_f()->beforeStop($this, ...\array_values($arguments));
        		
        		parent::stop(...\array_values($arguments));
        		break;
        	default:
        		$arguments = \func_get_args();
        		parent::stop(...\array_values($arguments));
        		break;
        }
    }

    /**
     * plugin "acme-p"
     * @return \Acme\Base\Plugin\P
     */
    private function ____plugin_acme_p()
    {
        if ($this->____plugin_acme_p === null) {
        	$this->____plugin_acme_p = $this->____om->get(\Acme\Base\Plugin\P::class);
        }
        return $this->____plugin_acme_p;
    }

    /**
     * plugin "acme_f"
     * @return \Acme\Base\Plugin\F
     */
    private function ____plugin_acme_f()
    {
        if ($this->____plugin_acme_f === null) {
        	$this->____plugin_acme_f = $this->____om->get(\Acme\Base\Plugin\F::class);
        }
        return $this->____plugin_acme_f;
    }
}
"#;
    if got != want {
        let _ = std::fs::write(dir.with_extension("got.php"), &got);
    }
    assert_eq!(got, want);

    let _ = std::fs::remove_dir_all(&dir);
}
