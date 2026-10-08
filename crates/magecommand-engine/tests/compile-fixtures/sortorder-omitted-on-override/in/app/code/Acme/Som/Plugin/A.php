<?php

namespace Acme\Som\Plugin;

class A
{
    public function afterAct($subject, int $result): int
    {
        return $result;
    }
}
