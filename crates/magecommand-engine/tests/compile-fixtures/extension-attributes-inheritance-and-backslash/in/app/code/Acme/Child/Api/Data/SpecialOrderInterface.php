<?php

namespace Acme\Child\Api\Data;

interface SpecialOrderInterface extends \Acme\Base\Api\Data\OrderInterface
{
    public function getPriority(): int;
}
