<?php
// Regenerates expected.ser with Magento's own Config\Dom and ObjectManager
// Mapper\Dom (the interpreters ObjectManagerFactory::createArgumentInterpreter
// composes):
//   php regenerate.php <magento>/vendor/autoload.php base.xml child.xml > expected.ser
// Fixture\C's constants are the ones tests/devupgrade.rs evaluates.
namespace Fixture {
    class C
    {
        const A = 'constant a';
        const N = 5;
    }
}

namespace {
    require $argv[1];
    use Magento\Framework\Data\Argument\Interpreter as I;

    $ids = [
        '/config/preference' => 'for',
        '/config/(type|virtualType)' => 'name',
        '/config/(type|virtualType)/plugin' => 'name',
        '/config/(type|virtualType)/arguments/argument' => 'name',
        '/config/(type|virtualType)/arguments/argument(/item)+' => 'name',
    ];
    $vs = new class implements \Magento\Framework\Config\ValidationStateInterface {
        public function isValidationRequired()
        {
            return false;
        }
    };
    $dom = new \Magento\Framework\Config\Dom(file_get_contents($argv[2]), $vs, $ids, 'xsi:type');
    $dom->merge(file_get_contents($argv[3]));
    $bool = new \Magento\Framework\Stdlib\BooleanUtils();
    $const = new I\Constant();
    $composite = new I\Composite([
        'boolean' => new I\Boolean($bool),
        'string' => new I\BaseStringUtils($bool),
        'number' => new I\Number(),
        'null' => new I\NullType(),
        'object' => new I\DataObject($bool),
        'const' => $const,
        'init_parameter' => new \Magento\Framework\App\Arguments\ArgumentInterpreter($const),
    ], 'xsi:type');
    $composite->addInterpreter('array', new I\ArrayType($composite));
    echo serialize((new \Magento\Framework\ObjectManager\Config\Mapper\Dom($composite, $bool))->convert($dom->getDom()));
}
